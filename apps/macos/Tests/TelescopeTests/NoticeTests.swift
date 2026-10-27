import XCTest

@testable import Telescope

final class NoticeTests: XCTestCase {
    func testTheLineNamesEveryKindOfFolderLeftOut() {
        let text = Notice.needsAccess.text
        XCTAssertTrue(text.contains("Desktop, Documents, Downloads and cloud drives"), text)
    }

    func testSettingsOpensOnFullDiskAccess() {
        XCTAssertEqual(
            Notice.settings.absoluteString,
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
    }

    func testTheProbedListIsMacOSOwnInTheHomeFolder() {
        let folder = (Hatch.permissionList as NSString).deletingLastPathComponent
        XCTAssertEqual(folder, NSHomeDirectory() + "/Library/Application Support/com.apple.TCC")
        XCTAssertTrue(FileManager.default.fileExists(atPath: folder), folder)
    }

    func testAReadableListMeansAccess() throws {
        let list = try temporaryFile()
        defer { try? FileManager.default.removeItem(atPath: list) }
        XCTAssertTrue(Hatch.hasFullDiskAccess(probing: list))
    }

    func testAMissingListMeansNoAccess() {
        XCTAssertFalse(Hatch.hasFullDiskAccess(probing: "/nonexistent/\(UUID().uuidString)/TCC.db"))
        XCTAssertFalse(Hatch.hasFullDiskAccess(probing: ""))
    }

    func testALockedListMeansNoAccess() throws {
        try XCTSkipIf(geteuid() == 0, "root opens any file")
        let list = try temporaryFile()
        defer { try? FileManager.default.removeItem(atPath: list) }
        try FileManager.default.setAttributes([.posixPermissions: 0], ofItemAtPath: list)
        XCTAssertFalse(Hatch.hasFullDiskAccess(probing: list))
    }

    func testProbingLeavesNoFileOpen() throws {
        let list = try temporaryFile()
        defer { try? FileManager.default.removeItem(atPath: list) }
        let before = try openFiles()
        for _ in 0..<1_000 { _ = Hatch.hasFullDiskAccess(probing: list) }
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
