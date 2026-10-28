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
                case .rebuilt, .opened, .reranked, .lines: XCTFail("nothing changed, saved, opened or read")
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

    func testHealthTellsTheFirstListThenARebuildAsked() async throws {
        let indexed = expectation(description: "indexed")
        let rebuilt = expectation(description: "rebuilt")
        let hatch = try XCTUnwrap(
            Hatch(standIn: 10_000) { event in
                switch event {
                case .indexed: indexed.fulfill()
                case .rebuilt(let count, _):
                    XCTAssertEqual(count, 10_000)
                    rebuilt.fulfill()
                case .failed: XCTFail("the core stopped")
                case .ready, .opened, .reranked, .lines: XCTFail("nothing searched, saved, opened or read")
                }
            })
        let started = Date(timeIntervalSince1970: Date().timeIntervalSince1970.rounded(.down))
        await fulfillment(of: [indexed], timeout: 5)
        let first = hatch.health()
        XCTAssertEqual(first.entries, 10_000)
        let scanned = try XCTUnwrap(first.scannedAt)
        XCTAssertGreaterThanOrEqual(scanned, started)
        XCTAssertLessThanOrEqual(scanned, Date())
        XCTAssertNil(first.changedAt, "no list after the first yet")
        XCTAssertFalse(first.busy)

        hatch.rebuild()
        XCTAssertTrue(hatch.health().busy, "asked for at once")
        await fulfillment(of: [rebuilt], timeout: 5)
        let second = hatch.health()
        XCTAssertNotNil(second.changedAt)
        // Times are whole seconds, so the core's own tests check a rebuild moves them on.
        XCTAssertNotNil(second.scannedAt)
        XCTAssertFalse(second.busy, "heard of only once its list is in use")
        XCTAssertFalse(second.behind || second.stopped)
        XCTAssertFalse(hatch.holds(folder: "/"), "made-up names have no folders on disk")
    }

    func testAnAppRowOpensItsBundle() {
        let app = Row(name: "Visual Studio Code", folder: "/Applications", isFolder: false, isApp: true)
        XCTAssertEqual(app.path, "/Applications/Visual Studio Code.app")
        XCTAssertTrue(app.url.hasDirectoryPath)
        XCTAssertEqual(app.url.path, app.path)
        let file = Row(name: "Visual Studio Code", folder: "/Applications", isFolder: false)
        XCTAssertEqual(file.path, "/Applications/Visual Studio Code")
        XCTAssertFalse(file.url.hasDirectoryPath)
        XCTAssertEqual(file.real, file.url)
    }

    /// The one link every Mac has: Safari lives on the system's own volume.
    func testAnAppRowReachedThroughALinkReadsTheAppItself() throws {
        let safari = Row(name: "Safari", folder: "/Applications", isFolder: false, isApp: true)
        let link = try FileManager.default.destinationOfSymbolicLink(atPath: safari.path)
        XCTAssertNotEqual(safari.real.path, safari.path, link)
        XCTAssertEqual(try safari.real.resourceValues(forKeys: [.contentTypeKey]).contentType, .applicationBundle)
    }

    func testASearchInsideAFolderFindsOnlyWhatIsInIt() async throws {
        let indexed = expectation(description: "indexed")
        var ready: [UInt64: XCTestExpectation] = [:]
        let hatch = try XCTUnwrap(
            Hatch(standIn: 10_000) { event in
                switch event {
                case .indexed: indexed.fulfill()
                case .ready(let ticket): ready[ticket]?.fulfill()
                case .failed: XCTFail("the core stopped")
                case .rebuilt, .opened, .reranked, .lines: XCTFail("nothing changed, saved, opened or read")
                }
            })
        await fulfillment(of: [indexed], timeout: 5)
        func rows(_ query: String, in folder: String?) async throws -> [Row] {
            let answered = expectation(description: query)
            let ticket = hatch.submit(query, in: folder)
            ready[ticket] = answered
            await fulfillment(of: [answered], timeout: 5)
            return try XCTUnwrap(hatch.take(ticket))
        }

        let folders = try await rows("budget", in: nil).filter(\.isFolder)
        let folder = try XCTUnwrap(folders.first).path
        let inside = try await rows("re", in: folder)
        XCTAssertFalse(inside.isEmpty)
        XCTAssertTrue(inside.allSatisfy { $0.folder == folder || $0.folder.hasPrefix(folder + "/") }, folder)
        XCTAssertFalse(inside.contains { $0.path == folder }, "the folder itself isn't inside it")
        let nowhere = try await rows("re", in: "/stand-in/nowhere")
        XCTAssertEqual(nowhere, [])
        let unicode = try await rows("re", in: "/stand-in/会議-報告")
        XCTAssertEqual(unicode, [])
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
                case .opened: XCTFail("nothing was saved")
                case .reranked, .lines: XCTFail("nothing was opened or read")
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
                case .opened: XCTFail("nothing was saved")
                case .reranked, .lines: XCTFail("nothing was opened or read")
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
        let gather = { (flags: [Int], ids: [FSEventStreamEventId]) in
            let flags = flags.map { FSEventStreamEventFlags($0) }
            return paths.withUnsafeBufferPointer { paths in
                flags.withUnsafeBufferPointer { flags in
                    ids.withUnsafeBufferPointer {
                        gatherChanges(flags.count, paths.baseAddress!, flags.baseAddress!, $0.baseAddress!)
                    }
                }
            }
        }
        let quiet = gather([kFSEventStreamEventFlagNone, kFSEventStreamEventFlagHistoryDone, 0], [40, 90, 41])
        XCTAssertEqual(quiet.bytes, Array("/a/\0/c/\0".utf8))
        XCTAssertFalse(quiet.rescan)
        XCTAssertEqual(quiet.newest, 41, "the end of a replay is no change")
        let none = paths.withUnsafeBufferPointer {
            gatherChanges(0, $0.baseAddress!, [FSEventStreamEventFlags(0)], [FSEventStreamEventId(5)])
        }
        let replayed = gather([kFSEventStreamEventFlagHistoryDone], [90])
        for nothing in [none, replayed] {
            XCTAssertEqual(nothing.bytes, [])
            XCTAssertFalse(nothing.rescan)
            XCTAssertEqual(nothing.newest, 0)
        }
        for dropped in [
            kFSEventStreamEventFlagMustScanSubDirs, kFSEventStreamEventFlagUserDropped,
            kFSEventStreamEventFlagKernelDropped, kFSEventStreamEventFlagEventIdsWrapped,
        ] {
            let lost = gather([0, dropped, 0], [7, 9, 8])
            XCTAssertTrue(lost.rescan)
            XCTAssertEqual(lost.newest, 9)
        }
    }

    func testTheJournalNamesTheFSEventsLogOfAVolume() throws {
        let home = try XCTUnwrap(Hatch.journal(of: NSHomeDirectory()))
        XCTAssertNotNil(UUID(uuidString: home), home)
        XCTAssertEqual(Hatch.journal(of: NSHomeDirectory() + "/Library"), home, "the same volume")
        XCTAssertNil(Hatch.journal(of: "/nowhere/\(UUID().uuidString)"))
        XCTAssertNil(Hatch.journal(of: "/dev"), "a volume FSEvents keeps no log for")
        XCTAssertTrue(Hatch.savedList.hasSuffix("/Library/Application Support/io.github.jovanhartono.telescope/index"))
    }

    func testAListSavedAtOneLaunchOpensAtTheNextAndKeepsUp() async throws {
        let (root, notes) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let support = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: support) }
        let file = support.appendingPathComponent("io.github.jovanhartono.telescope/index").path
        do {
            let (first, _) = try await watching(root, saved: file)
            // Saved 2 s after the first list.
            let deadline = ProcessInfo.processInfo.systemUptime + 10
            while !FileManager.default.fileExists(atPath: file) {
                guard ProcessInfo.processInfo.systemUptime < deadline else { return XCTFail("never saved") }
                try await Task.sleep(for: .milliseconds(50))
            }
            _ = first
        }
        let header = try XCTUnwrap(FileManager.default.contents(atPath: file))
        let savedAt = header[16..<24].reversed().reduce(UInt64(0)) { $0 << 8 | UInt64($1) }
        XCTAssertGreaterThan(savedAt, 0)
        XCTAssertLessThanOrEqual(savedAt, FSEventsGetCurrentEventId())

        // A file made while Telescope is closed is replayed from the saved event. Gone before
        // `support` is, so no save is under way as the folder goes.
        do {
            try Data().write(to: notes.appendingPathComponent("budget.txt"))
            let (hatch, counts) = try await watching(root, saved: file, opened: true)
            // The folder, notes, a.txt and budget.txt.
            try await counts.reach(4, within: 5)
            try Data().write(to: notes.appendingPathComponent("plan.md"))
            try await counts.reach(5, within: 5)
            _ = hatch
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

    func testIndexEverythingListsCachesWithoutARelaunchAndLeavesThemOutAgain() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let caches = root.appendingPathComponent("Library/Caches/telescope-test")
        try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
        try Data().write(to: caches.appendingPathComponent("found.txt"))
        let (hatch, counts) = try await watching(root)
        XCTAssertFalse(hatch.indexEverything(false), "already off")
        XCTAssertTrue(hatch.indexEverything(true))
        XCTAssertFalse(hatch.indexEverything(true), "asked once")
        // The folder, notes, a.txt, Library, Caches, telescope-test and found.txt.
        try await counts.reach(7, within: 10)
        hatch.watch()
        let on = try await names(hatch, "found")
        XCTAssertEqual(on, ["found.txt"])

        XCTAssertTrue(hatch.indexEverything(false))
        // Library itself stays listed.
        try await counts.reach(4, within: 10)
        let off = try await names(hatch, "found")
        XCTAssertEqual(off, [])
    }

    func testIndexEverythingAtLaunchListsCachesFromTheFirstList() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let caches = root.appendingPathComponent("Library/Caches")
        try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
        try Data().write(to: caches.appendingPathComponent("found.txt"))
        let (hatch, counts) = try await watching(root, everything: true)
        let found = try await names(hatch, "found")
        XCTAssertEqual(found, ["found.txt"])
        XCTAssertFalse(hatch.indexEverything(true), "the first list has it already")
        XCTAssertTrue(hatch.indexEverything(false))
        try await counts.reach(4, within: 10)
    }

    func testSwitchingBackBeforeTheRebuildStartsBuildsNothing() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, counts) = try await watching(root)
        // Within the core's half-second gap after the first list.
        XCTAssertTrue(hatch.indexEverything(true))
        XCTAssertFalse(hatch.indexEverything(false), "the list in use already skips clutter")
        XCTAssertFalse(hatch.health().building)
        try await Task.sleep(for: .seconds(1))
        XCTAssertEqual(counts.heard, 0)
    }

    /// The switch was kept on from a launch before access was granted.
    func testAccessGrantedWithTheSwitchOnBuildsOneList() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let caches = root.appendingPathComponent("Library/Caches")
        try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
        try Data().write(to: caches.appendingPathComponent("found.txt"))
        let (hatch, counts) = try await watching(root, fullDisk: false)
        hatch.grantFullDisk(everything: true)
        // The folder, notes, a.txt, Library, Caches and found.txt.
        try await counts.reach(6, within: 10)
        try await Task.sleep(for: .seconds(1))
        XCTAssertEqual(counts.heard, 1, "no second list")
        XCTAssertFalse(hatch.indexEverything(true), "already on")
    }

    /// The switch flipped and the app quit before the new list was saved: the saved list,
    /// built the other way, must not open.
    func testAListSavedWithTheSwitchTheOtherWayIsScannedAgain() async throws {
        for everything in [true, false] {
            let (root, _) = try folderWithNotes()
            defer { try? FileManager.default.removeItem(at: root) }
            let caches = root.appendingPathComponent("Library/Caches")
            try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
            try Data().write(to: caches.appendingPathComponent("found.txt"))
            let support = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            defer { try? FileManager.default.removeItem(at: support) }
            let file = support.appendingPathComponent("index").path
            do {
                let (first, _) = try await watching(root, everything: !everything, saved: file)
                let deadline = ProcessInfo.processInfo.systemUptime + 10
                while !FileManager.default.fileExists(atPath: file) {
                    guard ProcessInfo.processInfo.systemUptime < deadline else { return XCTFail("never saved") }
                    try await Task.sleep(for: .milliseconds(50))
                }
                _ = first
            }
            let (hatch, _) = try await watching(root, everything: everything, saved: file, opened: false)
            let found = try await names(hatch, "found")
            XCTAssertEqual(found, everything ? ["found.txt"] : [], "\(everything)")
        }
    }

    /// The names a search for `query` finds, once its answer comes.
    private func names(_ hatch: Hatch, _ query: String) async throws -> [String] {
        let ticket = hatch.submit(query)
        let deadline = ProcessInfo.processInfo.systemUptime + 5
        while true {
            if let rows = hatch.take(ticket) { return rows.map(\.name) }
            guard ProcessInfo.processInfo.systemUptime < deadline else { throw CancellationError() }
            try await Task.sleep(for: .milliseconds(10))
        }
    }

    func testVolumesTheCoreRefusesAreLeftOut() async throws {
        let (root, notes) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, counts) = try await watching(root)
        for path in ["/", "", "/Volumes", "/System/Volumes/Data", "/Volumes/No Such Drive", root.path, notes.path] {
            XCTAssertFalse(hatch.volumeAdded(path), path)
        }
        XCTAssertFalse(hatch.volumeRemoved("/Volumes/No Such Drive"), "never told of it")
        // Lists start at least 0.5 s apart, so one queued by mistake would be in by now.
        try await Task.sleep(for: .milliseconds(1200))
        XCTAssertEqual(counts.heard, 0, "no new list for a volume left out")
    }

    func testLettingGoOfADriveTheCoreNeverHeldLeavesTheWatchAlone() async throws {
        let (root, _) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let (hatch, _) = try await watching(root)
        let before = hatch.watching
        let notes = try XCTUnwrap(before.first { $0.hasSuffix("/notes") })
        XCTAssertFalse(hatch.volumeRemoved(notes))
        XCTAssertFalse(hatch.volumeRemoved(root.path))
        XCTAssertEqual(hatch.watching, before)
    }

    func testAChangeAtHomeDuringASwapOfTheWatchStillCounts() async throws {
        let (root, notes) = try folderWithNotes()
        defer { try? FileManager.default.removeItem(at: root) }
        let docs = root.appendingPathComponent("docs")
        try FileManager.default.createDirectory(at: docs, withIntermediateDirectories: false)
        let (hatch, counts) = try await watching(root)
        let watched = try XCTUnwrap(hatch.watching.first { $0.hasSuffix("/notes") })

        try Data().write(to: docs.appendingPathComponent("plan.md"))
        // Within the 0.2 s the old watch gathers changes for, so it never handed this one on.
        try await Task.sleep(for: .milliseconds(100))
        hatch.watch(hatch.watching.filter { $0 != watched }, since: hatch.heardUpTo)
        XCTAssertFalse(hatch.watching.contains(watched))
        // The folder, notes, a.txt, docs and plan.md.
        try await counts.reach(5, within: 5)

        try Data().write(to: notes.appendingPathComponent("unheard.txt"))
        try await Task.sleep(for: .milliseconds(1200))
        XCTAssertEqual(counts.newest, 5, "notes is no longer watched")
    }

    private func folderWithNotes() throws -> (URL, URL) {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: true)
        try Data().write(to: notes.appendingPathComponent("a.txt"))
        return (root, notes)
    }

    /// A core over `root`, keeping its list in `saved` when given, watched once its first list
    /// is in, scanned or `opened` from the file.
    private func watching(
        _ root: URL, fullDisk: Bool = true, everything: Bool = false, saved: String? = nil, opened: Bool = false
    ) async throws -> (Hatch, Counts) {
        let counts = Counts()
        let first = expectation(description: opened ? "opened" : "indexed")
        let hatch = try XCTUnwrap(
            Hatch(root: root.path, fullDisk: fullDisk, everything: everything, saved: saved) { event in
                switch event {
                case .indexed:
                    XCTAssertFalse(opened, "scanned in place of the saved list")
                    first.fulfill()
                case .opened:
                    XCTAssertTrue(opened, "opened a list that shouldn't open")
                    first.fulfill()
                case .rebuilt(let count, _): counts.add(count)
                case .ready: break
                case .failed: XCTFail("the scan failed")
                case .reranked, .lines: XCTFail("nothing was opened or read")
                }
            })
        await fulfillment(of: [first], timeout: 5)
        hatch.watch()
        return (hatch, counts)
    }
}

/// The entry count of each new list, in order.
@MainActor
private final class Counts {
    private var counts: [UInt64] = []
    var heard: Int { counts.count }
    var newest: UInt64? { counts.last }

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
