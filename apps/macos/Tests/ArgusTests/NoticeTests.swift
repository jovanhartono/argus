import XCTest

@testable import Argus

final class NoticeTests: XCTestCase {
    func testTheLineNamesEveryKindOfFolderLeftOut() {
        let text = Notice.needsAccess.text
        XCTAssertTrue(text.contains("Desktop, Documents, Downloads and cloud drives"), text)
    }

    func testSettingsOpensOnFullDiskAccess() {
        XCTAssertEqual(
            Notice.needsAccess.settings.absoluteString,
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
    }

    func testTheFinderLineOpensAutomation() {
        XCTAssertEqual(
            Notice.needsFinder.settings.absoluteString,
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation")
        XCTAssertTrue(Notice.needsFinder.text.contains("control Finder"), Notice.needsFinder.text)
    }

    /// Run from a terminal with Full Disk Access, as on the dev Mac, the real places say so.
    func testWithAccessTheRealPlacesSaySo() throws {
        try XCTSkipUnless(
            FileManager.default.isReadableFile(atPath: Hatch.guarded[1]), "this test runner lacks Full Disk Access")
        XCTAssertTrue(Hatch.hasFullDiskAccess())
        for place in Hatch.guarded where FileManager.default.fileExists(atPath: place) {
            XCTAssertTrue(Hatch.hasFullDiskAccess(probing: [place]), place)
        }
    }

    func testAFolderThatOpensMeansAccess() {
        XCTAssertTrue(Hatch.hasFullDiskAccess(probing: [FileManager.default.temporaryDirectory.path]))
    }

    func testAReadableListMeansAccess() throws {
        let list = try temporaryFile()
        defer { try? FileManager.default.removeItem(atPath: list) }
        XCTAssertTrue(Hatch.hasFullDiskAccess(probing: [list]))
    }

    func testAMissingListMeansNoAccess() {
        XCTAssertFalse(Hatch.hasFullDiskAccess(probing: ["/nonexistent/\(UUID().uuidString)/TCC.db"]))
        XCTAssertFalse(Hatch.hasFullDiskAccess(probing: [""]))
        XCTAssertFalse(Hatch.hasFullDiskAccess(probing: []))
    }

    /// As on macOS 27, where the user's own list is gone but the system's opens with access.
    func testAPlaceThatOpensAfterAMissingOneMeansAccess() throws {
        let list = try temporaryFile()
        defer { try? FileManager.default.removeItem(atPath: list) }
        XCTAssertTrue(Hatch.hasFullDiskAccess(probing: ["/nonexistent/\(UUID().uuidString)/TCC.db", list]))
    }

    func testALockedListMeansNoAccess() throws {
        try XCTSkipIf(geteuid() == 0, "root opens any file")
        let list = try temporaryFile()
        defer { try? FileManager.default.removeItem(atPath: list) }
        try FileManager.default.setAttributes([.posixPermissions: 0], ofItemAtPath: list)
        XCTAssertFalse(Hatch.hasFullDiskAccess(probing: [list, list + "-missing"]))
    }

    func testProbingLeavesNoFileOpen() throws {
        let list = try temporaryFile()
        defer { try? FileManager.default.removeItem(atPath: list) }
        let before = try openFiles()
        for _ in 0..<1_000 { _ = Hatch.hasFullDiskAccess(probing: [list]) }
        // Other tests' scan threads may open a file or two meanwhile; a leak would show 1,000.
        XCTAssertLessThan(try openFiles() - before, 10)
    }

    private func temporaryFile() throws -> String {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString).path
        try Data("SQLite format 3".utf8).write(to: URL(fileURLWithPath: path))
        return path
    }

    private func openFiles() throws -> Int {
        try FileManager.default.contentsOfDirectory(atPath: "/dev/fd").count
    }
}
