import XCTest

@testable import Telescope

@MainActor
final class HatchTests: XCTestCase {
    func testASearchComesBackAsRows() async throws {
        let indexed = expectation(description: "indexed")
        let ready = expectation(description: "ready")
        var answered: UInt64 = 0
        let hatch = try XCTUnwrap(
            Hatch(standIn: 10_000) { event in
                switch event {
                case .indexed: indexed.fulfill()
                case .ready(let ticket):
                    answered = ticket
                    ready.fulfill()
                case .failed: XCTFail("the core stopped")
                }
            })
        await fulfillment(of: [indexed], timeout: 5)

        let ticket = hatch.submit("readme")
        await fulfillment(of: [ready], timeout: 5)

        XCTAssertEqual(answered, ticket)
        let rows = try XCTUnwrap(hatch.take(ticket))
        XCTAssertFalse(rows.isEmpty)
        XCTAssertTrue(rows.allSatisfy { $0.name.lowercased().contains("readme") })
        XCTAssertNil(hatch.take(ticket), "rows are handed over once")
    }

    func testAScannedFolderComesBackAsRows() async throws {
        let files = FileManager.default
        let root = files.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? files.removeItem(at: root) }
        let notes = root.appendingPathComponent("notes")
        try files.createDirectory(at: notes, withIntermediateDirectories: true)
        try Data().write(to: notes.appendingPathComponent("budget 2026.txt"))

        let indexed = expectation(description: "indexed")
        let ready = expectation(description: "ready")
        var count: UInt64 = 0
        let hatch = try XCTUnwrap(
            Hatch(root: root.path, fullDisk: Hatch.hasFullDiskAccess()) { event in
                switch event {
                case .indexed(let entries):
                    count = entries
                    indexed.fulfill()
                case .ready: ready.fulfill()
                case .failed: XCTFail("the scan failed")
                }
            })
        await fulfillment(of: [indexed], timeout: 5)
        XCTAssertEqual(count, 3, "the folder, notes and the budget")

        let ticket = hatch.submit("budget")
        await fulfillment(of: [ready], timeout: 5)
        let rows = try XCTUnwrap(hatch.take(ticket))
        XCTAssertEqual(rows.map(\.name), ["budget 2026.txt"])
        // Rust reports the real path, /private/var/…, which Foundation shortens to /var/….
        XCTAssertTrue(rows.first?.folder.hasSuffix("/\(root.lastPathComponent)/notes") ?? false)
    }
}
