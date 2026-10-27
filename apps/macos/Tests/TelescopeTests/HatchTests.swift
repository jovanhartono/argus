import CoreServices
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
                case .rebuilt: XCTFail("nothing changed")
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
                case .rebuilt: break
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

    func testAFileSavedInAWatchedFolderBringsANewList() async throws {
        let (root, notes) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, counts) = try await watching(root)
        try Data().write(to: notes.appendingPathComponent("budget.txt"))
        // The folder, notes, a.txt and budget.txt.
        try await counts.reach(4, within: 5)
        _ = hatch
    }

    func testAFolderMadeAtTheTopIsNoticedAndWatched() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, counts) = try await watching(root)
        let work = root.appendingPathComponent("work")
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: false)
        try await counts.reach(4, within: 5)
        hatch.watch()
        try Data().write(to: work.appendingPathComponent("plan.md"))
        try await counts.reach(5, within: 5)
    }

    func testAChangeMissedByTheNewListIsReplayedOnceItsFolderIsWatched() async throws {
        let (root, notes) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, counts) = try await watching(root)
        let work = root.appendingPathComponent("work")
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: false)
        try await counts.reach(4, within: 5)
        // Before work is watched: a file in it, then a save the old stream hears after it.
        try Data().write(to: work.appendingPathComponent("plan.md"))
        try Data("edited".utf8).write(to: notes.appendingPathComponent("a.txt"))
        try await Task.sleep(for: .seconds(1))
        hatch.watch()
        try await counts.reach(5, within: 5)
    }

    func testChangesMadeWhileTheFirstListIsBuiltStillCount() async throws {
        let (root, notes) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let counts = Counts()
        let indexed = expectation(description: "indexed")
        var first: UInt64 = 0
        let hatch = try XCTUnwrap(
            Hatch(root: root.path, fullDisk: true) { event in
                switch event {
                case .indexed(let count):
                    first = count
                    indexed.fulfill()
                case .rebuilt(let count, _): counts.add(count)
                case .ready: break
                case .failed: XCTFail("the scan failed")
                }
            })
        // The scan may or may not have read them yet; either way the list ends up with them.
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("work"), withIntermediateDirectories: false)
        try Data().write(to: notes.appendingPathComponent("budget.txt"))
        await fulfillment(of: [indexed], timeout: 5)
        hatch.watch()
        // The folder, notes, a.txt, budget.txt and work.
        if first != 5 { try await counts.reach(5, within: 5) }
    }

    func testAFolderAppearingInLibraryIsNoticed() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let library = root.appendingPathComponent("Library")
        try FileManager.default.createDirectory(
            at: library.appendingPathComponent("Caches"), withIntermediateDirectories: true)
        let (hatch, counts) = try await watching(root)
        let cloud = library.appendingPathComponent("CloudStorage")
        try FileManager.default.createDirectory(at: cloud, withIntermediateDirectories: false)
        // The folder, notes, a.txt, Library and CloudStorage.
        try await counts.reach(5, within: 5)
        hatch.watch()
        try Data().write(to: cloud.appendingPathComponent("plan.md"))
        try await counts.reach(6, within: 5)
        try FileManager.default.moveItem(at: cloud, to: library.appendingPathComponent("Old"))
        try await counts.reach(4, within: 5)
    }

    func testSavingTheIgnoreListBringsANewList() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, counts) = try await watching(root)
        let list = root.appendingPathComponent(".config/telescope")
        try FileManager.default.createDirectory(at: list, withIntermediateDirectories: true)
        try Data("notes\n".utf8).write(to: list.appendingPathComponent("ignore"))
        // The folder alone.
        try await counts.reach(1, within: 5)
        _ = hatch
    }

    func testEventsFromAFreedCoreNeverReachTheNextOne() async throws {
        var old = Hatch(standIn: 100) { _ in }
        _ = old?.submit("readme")
        // Its events wait on the main queue meanwhile.
        usleep(300_000)
        old = nil
        var heard: [Hatch.Event] = []
        let indexed = expectation(description: "indexed")
        let hatch = try XCTUnwrap(
            Hatch(standIn: 200) { event in
                heard.append(event)
                if case .indexed = event { indexed.fulfill() }
            })
        await fulfillment(of: [indexed], timeout: 5)
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(heard, [.indexed(count: 200)])
        _ = hatch
    }

    func testDroppedChangesAskForARescanAndTheEndOfAReplayIsSkipped() throws {
        let paths: [UnsafePointer<CChar>] = ["/a/", "/b/", "/c/"].map { (path: String) in
            UnsafePointer(strdup(path)!)
        }
        defer { paths.forEach { free(UnsafeMutablePointer(mutating: $0)) } }
        let gather = { (flags: [Int]) in
            let flags = flags.map { FSEventStreamEventFlags($0) }
            return paths.withUnsafeBufferPointer { paths in
                flags.withUnsafeBufferPointer { gatherChanges(3, paths.baseAddress!, $0.baseAddress!) }
            }
        }
        let quiet = gather([kFSEventStreamEventFlagNone, kFSEventStreamEventFlagHistoryDone, 0])
        XCTAssertEqual(quiet.bytes, Array("/a/\0/c/\0".utf8))
        XCTAssertFalse(quiet.rescan)
        for dropped in [
            kFSEventStreamEventFlagMustScanSubDirs, kFSEventStreamEventFlagUserDropped,
            kFSEventStreamEventFlagKernelDropped, kFSEventStreamEventFlagEventIdsWrapped,
        ] {
            XCTAssertTrue(gather([0, dropped, 0]).rescan)
        }
    }

    func testChangesOutsideTheListAreIgnored() async throws {
        let (root, notes) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, counts) = try await watching(root)
        let modules = notes.appendingPathComponent("node_modules")
        try FileManager.default.createDirectory(at: modules, withIntermediateDirectories: false)
        for file in 0..<20 { try Data().write(to: modules.appendingPathComponent("\(file).js")) }
        // Saved over: the same names as before.
        try Data("edited".utf8).write(to: notes.appendingPathComponent("a.txt"))
        try await Task.sleep(for: .seconds(1.5))
        XCTAssertEqual(counts.heard, 0)

        try Data().write(to: notes.appendingPathComponent("budget.txt"))
        try await counts.reach(4, within: 5)
        _ = hatch
    }

    private func folderWithNotes() throws -> (URL, URL) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: true)
        try Data().write(to: notes.appendingPathComponent("a.txt"))
        return (root, notes)
    }

    private func watching(_ root: URL) async throws -> (Hatch, Counts) {
        let counts = Counts()
        let indexed = expectation(description: "indexed")
        let hatch = try XCTUnwrap(
            Hatch(root: root.path, fullDisk: true) { event in
                switch event {
                case .indexed: indexed.fulfill()
                case .rebuilt(let count, _): counts.add(count)
                case .ready: break
                case .failed: XCTFail("the scan failed")
                }
            })
        await fulfillment(of: [indexed], timeout: 5)
        hatch.watch()
        return (hatch, counts)
    }
}

/// The entry count of each new list, in order.
@MainActor
private final class Counts {
    private var counts: [UInt64] = []
    var heard: Int { counts.count }

    func add(_ count: UInt64) { counts.append(count) }

    /// Waits for a new list of `count` entries.
    func reach(_ count: UInt64, within seconds: Double) async throws {
        let deadline = ProcessInfo.processInfo.systemUptime + seconds
        while counts.last != count {
            guard ProcessInfo.processInfo.systemUptime < deadline else {
                XCTFail("no list of \(count) within \(seconds) s, only \(counts)")
                throw CancellationError()
            }
            try await Task.sleep(for: .milliseconds(10))
        }
    }
}
