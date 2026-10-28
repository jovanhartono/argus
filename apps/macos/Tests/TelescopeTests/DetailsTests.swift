import UniformTypeIdentifiers
import XCTest

@testable import Telescope

final class DetailsTests: XCTestCase {
    private let us = DetailsFormat(locale: Locale(identifier: "en_US"), timeZone: TimeZone(identifier: "UTC")!)
    private var root: URL!

    override func setUp() async throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: root)
    }

    private func file(_ type: UTType = .pdf, size: Int64?, found: Bool = true) -> Details {
        Details(type: type, size: size, created: nil, modified: nil, found: found)
    }

    /// Sizes follow this Mac's own number format, so the decimal mark is the one it uses.
    func testSizesReadLikeFinders() {
        let mark = Locale.current.decimalSeparator ?? "."
        let sizes: [(Int64, String)] = [
            (0, "0 bytes"), (1, "1 byte"), (999, "999 bytes"), (1_000, "1 KB"), (1_499, "1 KB"),
            (1_234_567, "1\(mark)2 MB"), (5_000_000_000, "5 GB"), (3_200_000_000_000, "3\(mark)2 TB"),
        ]
        for (bytes, text) in sizes {
            XCTAssertEqual(us.summary(file(size: bytes)), "PDF document – \(text)", "\(bytes)")
        }
    }

    func testFindersOwnKindWinsOverTheTypeName() {
        let details = Details(type: .folder, kind: "Folder", size: nil, created: nil, modified: nil, found: true)
        XCTAssertEqual(us.summary(details), "Folder")
        let app = Details(type: .applicationBundle, kind: "Application", size: nil, created: nil, modified: nil, found: true)
        XCTAssertEqual(us.summary(app), "Application")
    }

    func testAFolderHasNoSize() {
        XCTAssertEqual(us.summary(file(.folder, size: nil)), "Folder")
    }

    func testAFileGoneSaysSoInsteadOfASize() {
        XCTAssertEqual(us.summary(file(size: nil, found: false)), "PDF document – can’t be found")
        XCTAssertEqual(us.summary(file(size: 12, found: false)), "PDF document – can’t be found")
    }

    func testAKindMacOSDoesntKnowStillHasAName() {
        let made = UTType(filenameExtension: "zzqq")!
        let text = us.summary(file(made, size: 3))
        XCTAssertTrue(text.hasSuffix(" – 3 bytes"), text)
        XCTAssertFalse(text.hasPrefix(" "), text)
    }

    func testRecentDatesSayTodayAndYesterday() {
        let now = Date()
        XCTAssertTrue(us.date(now).hasPrefix("Today at "), us.date(now))
        let yesterday = now.addingTimeInterval(-86_400)
        XCTAssertTrue(us.date(yesterday).hasPrefix("Yesterday at "), us.date(yesterday))
        let lastWeek = now.addingTimeInterval(-7 * 86_400)
        XCTAssertFalse(us.date(lastWeek).hasPrefix("Yesterday"), us.date(lastWeek))
    }

    func testOlderDatesSpellTheDayAndTime() {
        let date = Date(timeIntervalSince1970: 1_709_647_380)  // 5 Mar 2024, 14:03 UTC
        XCTAssertEqual(us.date(date), "Mar 5, 2024 at 2:03\u{202F}PM")
        let tokyo = DetailsFormat(locale: Locale(identifier: "en_US"), timeZone: TimeZone(identifier: "Asia/Tokyo")!)
        XCTAssertEqual(tokyo.date(date), "Mar 5, 2024 at 11:03\u{202F}PM", "in your own time zone")
    }

    func testAnUnknownDateIsADash() {
        XCTAssertEqual(us.date(nil), "—")
    }

    func testFoldersInTheHomeFolderStartWithATilde() {
        let home = "/Users/jan"
        XCTAssertEqual(us.folder("/Users/jan", home: home), "~")
        XCTAssertEqual(us.folder("/Users/jan/Documents/notes", home: home), "~/Documents/notes")
        XCTAssertEqual(us.folder("/Users/janet/Documents", home: home), "/Users/janet/Documents", "another user")
        XCTAssertEqual(us.folder("/Applications", home: home), "/Applications")
        XCTAssertEqual(us.folder("", home: home), "")
    }

    func testAFileOnDiskGivesItsSizeKindAndDates() throws {
        let url = root.appendingPathComponent("notes.txt")
        try Data("twelve bytes".utf8).write(to: url)
        let modified = Date(timeIntervalSince1970: 1_709_647_380)
        try FileManager.default.setAttributes([.modificationDate: modified], ofItemAtPath: url.path)
        let details = Details.read(Row(name: "notes.txt", folder: root.path, isFolder: false))
        XCTAssertTrue(details.found)
        XCTAssertEqual(details.size, 12)
        XCTAssertEqual(details.type, .plainText)
        XCTAssertEqual(details.modified, modified)
        XCTAssertNotNil(details.created)
    }

    func testAnEmptyFileHasSizeZero() throws {
        try Data().write(to: root.appendingPathComponent("empty.pdf"))
        let details = Details.read(Row(name: "empty.pdf", folder: root.path, isFolder: false))
        XCTAssertEqual(details.size, 0)
        XCTAssertEqual(us.summary(details), "PDF document – 0 bytes")
    }

    func testAFileYouCantOpenStillHasItsDetails() throws {
        let url = root.appendingPathComponent("locked.txt")
        try Data("abc".utf8).write(to: url)
        try FileManager.default.setAttributes([.posixPermissions: 0], ofItemAtPath: url.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o644], ofItemAtPath: url.path) }
        let details = Details.read(Row(name: "locked.txt", folder: root.path, isFolder: false))
        XCTAssertTrue(details.found)
        XCTAssertEqual(details.size, 3)
    }

    func testAFolderAndAnAppAreReadWithoutASize() throws {
        let folder = root.appendingPathComponent("photos")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: false)
        try Data(count: 4096).write(to: folder.appendingPathComponent("big.jpg"))
        let details = Details.read(Row(name: "photos", folder: root.path, isFolder: true))
        XCTAssertEqual(details.type, .folder)
        XCTAssertNil(details.size)
        XCTAssertEqual(us.summary(details), "Folder")

        let app = root.appendingPathComponent("Fake.app")
        try FileManager.default.createDirectory(at: app, withIntermediateDirectories: false)
        let bundle = Details.read(Row(name: "Fake.app", folder: root.path, isFolder: true))
        XCTAssertTrue(bundle.type.conforms(to: .application), bundle.type.identifier)
        XCTAssertNil(bundle.size)
    }

    func testTheIndexedFolderListedByItsFullPathIsRead() {
        let details = Details.read(Row(name: root.path, folder: "", isFolder: true))
        XCTAssertTrue(details.found)
        XCTAssertEqual(details.type, .folder)
    }

    @MainActor
    func testThePaneSaysWhereTheIndexedFolderItselfLives() {
        let pane = DetailsPane(format: us)
        pane.show(Row(name: root.path, folder: "", isFolder: true), scale: 2)
        XCTAssertEqual(pane.name.stringValue, root.path)
        XCTAssertEqual(pane.place.stringValue, root.deletingLastPathComponent().path)
        XCTAssertEqual(pane.summary.stringValue, "Folder")
        pane.stopThumbnail()
    }

    @MainActor
    func testThePaneReadsTheFileAgainButKeepsItsThumbnailWhenNothingChanged() throws {
        let url = root.appendingPathComponent("notes.txt")
        try Data("abc".utf8).write(to: url)
        let row = Row(name: "notes.txt", folder: root.path, isFolder: false)
        let pane = DetailsPane(format: us)
        pane.show(row, scale: 2)
        XCTAssertEqual(pane.waiting, url)
        pane.stopThumbnail()
        pane.show(row, scale: 2)
        XCTAssertNil(pane.waiting, "the same file unchanged asks for no new thumbnail")

        try Data("abcdef".utf8).write(to: url)
        try FileManager.default.setAttributes([.modificationDate: Date(timeIntervalSinceNow: 60)], ofItemAtPath: url.path)
        pane.show(row, scale: 2)
        XCTAssertEqual(pane.summary.stringValue, "Plain Text Document – 6 bytes")
        XCTAssertEqual(pane.waiting, url, "a changed file gets a new thumbnail")
        pane.show(nil, scale: 2)
        XCTAssertNil(pane.waiting, "nothing picked gives up the thumbnail")
    }

    func testAFileGoneIsGuessedFromItsName() {
        let gone = Details.read(Row(name: "report.pdf", folder: root.path, isFolder: false))
        XCTAssertFalse(gone.found)
        XCTAssertEqual(gone.type, .pdf)
        XCTAssertNil(gone.size)
        XCTAssertNil(gone.modified)
        XCTAssertEqual(Details.read(Row(name: "old", folder: root.path, isFolder: true)).type, .folder)
        XCTAssertEqual(Details.read(Row(name: "README", folder: root.path, isFolder: false)).type, .data)
    }
}
