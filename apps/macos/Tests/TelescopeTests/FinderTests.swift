import CoreServices
import XCTest

@testable import Telescope

/// Finder's replies are built by hand here: asking the real Finder would bring up macOS's
/// question about controlling it.
final class FinderTests: XCTestCase {
    private var root: URL!

    override func setUp() async throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: root)
    }

    private var real: String {
        String(cString: realpath(root.path, nil))
    }

    private func reply(_ answer: NSAppleEventDescriptor? = nil, error: Int32? = nil) -> NSAppleEventDescriptor {
        let reply = NSAppleEventDescriptor(
            eventClass: AEEventClass(kCoreEventClass), eventID: AEEventID(kAEAnswer), targetDescriptor: nil,
            returnID: AEReturnID(kAutoGenerateReturnID), transactionID: AETransactionID(kAnyTransactionID))
        if let answer { reply.setParam(answer, forKeyword: AEKeyword(keyDirectObject)) }
        if let error { reply.setParam(NSAppleEventDescriptor(int32: error), forKeyword: AEKeyword(keyErrorNumber)) }
        return reply
    }

    func testAFolderComesBackAsTheRealPathTheListHolds() throws {
        // macOS's temporary folder sits under /var, a link to /private/var.
        XCTAssertNotEqual(root.path, real)
        let answer = NSAppleEventDescriptor(fileURL: root)
        XCTAssertEqual(FinderFolder.read { reply(answer) }, .folder(real))
        let alias = try XCTUnwrap(answer.coerce(toDescriptorType: DescType(typeAlias)))
        XCTAssertEqual(FinderFolder.read { reply(alias) }, .folder(real))
    }

    func testAFolderReachedThroughALinkIsItsTarget() throws {
        let notes = root.appendingPathComponent("notes 会議")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        let link = root.appendingPathComponent("shortcut")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: notes)
        XCTAssertEqual(FinderFolder.folder(at: link), .folder(real + "/notes 会議"))
        XCTAssertEqual(FinderFolder.folder(at: URL(fileURLWithPath: "/")), .folder("/"))
    }

    func testAnythingButAFolderOnDiskIsNoFolder() throws {
        let file = root.appendingPathComponent("budget.txt")
        try Data().write(to: file)
        XCTAssertEqual(FinderFolder.folder(at: file), .none)
        XCTAssertEqual(FinderFolder.folder(at: root.appendingPathComponent("gone")), .none)
        XCTAssertEqual(FinderFolder.folder(at: URL(string: "https://example.com/folder")!), .none)
        XCTAssertEqual(FinderFolder.read { reply() }, .none, "no answer")
        XCTAssertEqual(FinderFolder.read { reply(NSAppleEventDescriptor(int32: 7)) }, .none)
        // Finder's URL property is text, not a file.
        let text = NSAppleEventDescriptor(string: root.absoluteString)
        XCTAssertEqual(FinderFolder.read { reply(text) }, .none)
    }

    func testErrorsInTheReplyOrFromSendingAreSortedOut() {
        // Finder window 1 when none is open, as Finder really answers.
        XCTAssertEqual(FinderFolder.read { reply(error: Int32(errAENoSuchObject)) }, .none)
        XCTAssertEqual(FinderFolder.read { reply(error: Int32(errAECoercionFail)) }, .none)
        XCTAssertEqual(FinderFolder.read { reply(error: Int32(errAEEventNotPermitted)) }, .denied)
        XCTAssertEqual(FinderFolder.read { reply(error: Int32(errAETimeout)) }, .failed(Int(errAETimeout)))
        XCTAssertEqual(
            FinderFolder.read { reply(NSAppleEventDescriptor(fileURL: root), error: Int32(errAENoSuchObject)) }, .none,
            "an error wins over an answer")
        XCTAssertEqual(FinderFolder.read { reply(NSAppleEventDescriptor(fileURL: root), error: 0) }, .folder(real))
        let thrown = { (code: Int) in
            FinderFolder.read { throw NSError(domain: NSOSStatusErrorDomain, code: code) }
        }
        XCTAssertEqual(thrown(Int(errAEEventNotPermitted)), .denied)
        XCTAssertEqual(thrown(Int(procNotFound)), .none)
        XCTAssertEqual(thrown(Int(errAETimeout)), .failed(Int(errAETimeout)))
    }

    func testTheQuestionIsTheTargetOfTheFrontFinderWindowAsAnAlias() throws {
        let finder = NSAppleEventDescriptor(bundleIdentifier: "com.apple.finder")
        let event = FinderFolder.frontTarget(of: finder)
        XCTAssertEqual(event.eventClass, AEEventClass(kAECoreSuite))
        XCTAssertEqual(event.eventID, AEEventID(kAEGetData))
        let address = try XCTUnwrap(event.attributeDescriptor(forKeyword: AEKeyword(keyAddressAttr)))
        XCTAssertEqual(address.descriptorType, DescType(typeApplicationBundleID))
        XCTAssertEqual(String(decoding: address.data, as: UTF8.self), "com.apple.finder")
        XCTAssertEqual(
            event.paramDescriptor(forKeyword: AEKeyword(keyAERequestedType))?.typeCodeValue, DescType(typeAlias))

        let target = try XCTUnwrap(event.paramDescriptor(forKeyword: AEKeyword(keyDirectObject)))
        XCTAssertEqual(target.descriptorType, DescType(typeObjectSpecifier))
        let field = { (spec: NSAppleEventDescriptor, key: Int) in spec.forKeyword(AEKeyword(key)) }
        XCTAssertEqual(field(target, keyAEDesiredClass)?.typeCodeValue, DescType(typeProperty))
        XCTAssertEqual(field(target, keyAEKeyForm)?.enumCodeValue, DescType(formPropertyID))
        XCTAssertEqual(field(target, keyAEKeyData)?.typeCodeValue, FinderFolder.code("fvtg"))
        let window = try XCTUnwrap(field(target, keyAEContainer))
        XCTAssertEqual(field(window, keyAEDesiredClass)?.typeCodeValue, FinderFolder.code("brow"))
        XCTAssertEqual(field(window, keyAEKeyForm)?.enumCodeValue, DescType(formAbsolutePosition))
        XCTAssertEqual(field(window, keyAEKeyData)?.int32Value, 1)
        XCTAssertEqual(field(window, keyAEContainer)?.descriptorType, DescType(typeNull))
    }

    func testFourLetterCodesReadAsFinderWritesThem() {
        XCTAssertEqual(FinderFolder.code("brow"), 0x6272_6F77)
        XCTAssertEqual(FinderFolder.code("fvtg"), 0x6676_7467)
        XCTAssertEqual(FinderFolder.code("alis"), DescType(typeAlias))
    }

    func testAFolderBecomesTheRowTabIntoItWouldMake() {
        let notes = Row(folderAt: "/Users/me/notes")
        XCTAssertEqual(notes, Row(name: "notes", folder: "/Users/me", isFolder: true))
        XCTAssertEqual(notes.path, "/Users/me/notes")
        XCTAssertEqual(Row(folderAt: "/Users").path, "/Users")
        let disk = Row(folderAt: "/")
        XCTAssertEqual(disk.path, "/")
        XCTAssertEqual(disk.name, "/")
        let odd = Row(folderAt: "/Volumes/Photos 2026/100% #会議")
        XCTAssertEqual(odd.name, "100% #会議")
        XCTAssertEqual(odd.path, "/Volumes/Photos 2026/100% #会議")
    }
}
