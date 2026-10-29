import XCTest

@testable import Argus

@MainActor
final class HatchTextTests: XCTestCase {
    private var root: URL!
    private var notes: URL!
    private var told: [UInt64] = []
    private var hatch: Hatch!

    override func setUp() async throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: true)
        try "café crème budget ✓\nnothing here\n    🚀 Budget rocket\n".write(
            to: notes.appendingPathComponent("menu.txt"), atomically: true, encoding: .utf8)
        hatch = try XCTUnwrap(
            Hatch(root: root.path, fullDisk: false) { [weak self] event in
                if case .lines(let ticket) = event { self?.told.append(ticket) }
            })
    }

    override func tearDown() async throws {
        hatch = nil
        try? FileManager.default.removeItem(at: root)
    }

    func testLinesComeBackWithTheMatchCountedAsAppKitCounts() async throws {
        let (hits, end) = try await found("budget", in: [root.path])
        XCTAssertEqual(hits.map(\.line), [1, 3])
        XCTAssertEqual(hits.map(\.text), ["café crème budget ✓", "🚀 Budget rocket"], "spaces around a line are dropped")
        XCTAssertEqual(hits.map(\.match), [NSRange(location: 11, length: 6), NSRange(location: 3, length: 6)])
        XCTAssertEqual(hits.map { ($0.text as NSString).substring(with: $0.match) }, ["budget", "Budget"])
        XCTAssertTrue(hits.allSatisfy { $0.path.hasSuffix("/\(root.lastPathComponent)/notes/menu.txt") })
        XCTAssertEqual(hits.first?.file.name, "menu.txt")
        XCTAssertFalse(hits.first?.file.isFolder ?? true)
        XCTAssertFalse(end.failed)
        XCTAssertEqual(end.filesSearched, 1)
    }

    func testCapitalsMustMatchExactly() async throws {
        let (hits, _) = try await found("Budget", in: [root.path])
        XCTAssertEqual(hits.map(\.line), [3])
    }

    func testAFolderInsideAnotherIsReadOnce() async throws {
        let (hits, end) = try await found("budget", in: [notes.path, root.path, notes.path])
        XCTAssertEqual(hits.count, 2)
        XCTAssertEqual(end.filesSearched, 1)
    }

    func testNoFolderAMissingOneOrNothingToFindEndsEmpty() async throws {
        let asks: [(String, [String])] = [
            ("budget", []), ("budget", [root.appendingPathComponent("gone").path]), ("zebra", [root.path]),
        ]
        for (query, folders) in asks {
            let (hits, end) = try await found(query, in: folders)
            XCTAssertEqual(hits, [], "\(query) in \(folders)")
            XCTAssertTrue(end.done)
        }
    }

    func testOnlyTheNewestSearchHandsLinesOver() async throws {
        let older = hatch.searchText("budget", in: [root.path])
        let newer = hatch.searchText("rocket", in: [root.path])
        XCTAssertNotEqual(older, 0)
        XCTAssertNil(hatch.takeText(older))
        let (hits, _) = try await drain(newer)
        XCTAssertEqual(hits.map(\.line), [3])
        XCTAssertNil(hatch.takeText(newer), "the end is handed over once")
        XCTAssertNil(hatch.takeText(0))
    }

    func testACancelledSearchHandsNothingOverAndTheNextOneRuns() async throws {
        let cancelled = hatch.searchText("budget", in: [root.path])
        hatch.cancelText()
        XCTAssertNil(hatch.takeText(cancelled))
        let (hits, end) = try await found("rocket", in: [root.path])
        XCTAssertEqual(hits.map(\.text), ["🚀 Budget rocket"], "no line of the stopped search comes with it")
        XCTAssertEqual(end.filesSearched, 1)
    }

    func testALongLineIsCutAroundTheMatchWithItsPlaceCountedAsAppKitCounts() async throws {
        let before = String(repeating: "é ", count: 150)
        let line = before + "budget" + String(repeating: " ✓", count: 40)
        try line.write(to: notes.appendingPathComponent("long.txt"), atomically: true, encoding: .utf8)
        let (hits, _) = try await found("budget", in: [notes.path])
        let hit = try XCTUnwrap(hits.first { $0.path.hasSuffix("/long.txt") })
        XCTAssertLessThanOrEqual(hit.text.utf8.count, 200)
        XCTAssertLessThan(hit.text.utf16.count, line.utf16.count, "cut")
        XCTAssertEqual((hit.text as NSString).substring(with: hit.match), "budget")
        XCTAssertGreaterThan(hit.match.location, 0, "a little of what comes before stays")
    }

    private func found(_ query: String, in folders: [String]) async throws -> ([TextHit], TextBatch) {
        let ticket = hatch.searchText(query, in: folders)
        XCTAssertNotEqual(ticket, 0)
        return try await drain(ticket)
    }

    /// Takes each batch as it is announced, until the end.
    private func drain(_ ticket: UInt64) async throws -> ([TextHit], TextBatch) {
        var hits: [TextHit] = []
        for _ in 0..<500 {
            if let at = told.firstIndex(of: ticket) {
                told.remove(at: at)
                let batch = try XCTUnwrap(hatch.takeText(ticket))
                hits += batch.hits
                if batch.done { return (hits.sorted { $0.line < $1.line }, batch) }
            }
            try await Task.sleep(for: .milliseconds(10))
        }
        XCTFail("the search never ended")
        return (hits, TextBatch())
    }
}
