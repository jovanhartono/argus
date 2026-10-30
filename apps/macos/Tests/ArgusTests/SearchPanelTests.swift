import AppKit
import QuickLookUI
import XCTest

@testable import Argus

@MainActor
final class SearchPanelTests: XCTestCase {
    private var root: URL!
    private var access = false
    private var checks = 0
    private var stage: OffStage!
    private var onScreen = false

    override func setUp() async throws {
        _ = NSApplication.shared
        stage = OffStage()
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: root)
        // Checked before the cleanup below, which would hide a window that slipped past the stand-in.
        if !onScreen {
            XCTAssertEqual(NSApp.windows.filter(\.isVisible), [], "a test put a window on the screen")
            XCTAssertFalse(NSApp.isActive, "a test made the test runner the active app")
        }
        // Every test shares the one preview; a failed test mustn't leave it up for the next.
        // A close already under way takes about 0.2 s, and a second one would cancel it.
        if QLPreviewPanel.sharedPreviewPanelExists(), let preview = QLPreviewPanel.shared() {
            if await !soon({ !preview.isVisible }) {
                preview.orderOut(nil)
                _ = await soon { !preview.isVisible }
            }
            preview.dataSource = nil
            preview.delegate = nil
        }
        // The tests after this one run in the same app and expect someone else in front. The search
        // window is freed before its fade-out ends, so nothing else takes it off the screen.
        if onScreen {
            for window in NSApp.windows where window.isVisible { window.orderOut(nil) }
            NSApp.deactivate()
            _ = await soon { !NSApp.isActive }
        }
    }

    func testWithoutAccessTheLineAndButtonShowUnderTheBox() async throws {
        let window = try await opened(access: false)
        XCTAssertEqual(window.notice, .needsAccess)
        XCTAssertFalse(window.settings.isHidden)
        XCTAssertEqual(window.panel.frame.height, Metric.field + 1 + Metric.message)
        assertFits(window)
    }

    func testWithAccessThereIsNoLineAndNoMoreChecks() async throws {
        let window = try await opened(access: true)
        XCTAssertNil(window.notice)
        XCTAssertEqual(window.panel.frame.height, Metric.field)
        window.recheckAccess()
        XCTAssertEqual(checks, 1, "with access there is nothing more to check")
    }

    func testAccessGrantedWhileRunningBringsInTheGuardedFoldersWithoutARelaunch() async throws {
        let documents = root.appendingPathComponent("Documents")
        try FileManager.default.createDirectory(at: documents, withIntermediateDirectories: false)
        try Data().write(to: documents.appendingPathComponent("budget.pdf"))
        let window = try await opened(access: false)
        try await type("budget", in: window)
        // The line, then No Results.
        try await until { window.panel.frame.height == Metric.field + 2 * (1 + Metric.message) }
        window.recheckAccess()
        XCTAssertEqual(window.notice, .needsAccess)

        access = true
        window.recheckAccess()
        XCTAssertNil(window.notice)
        XCTAssertTrue(window.settings.isHidden)
        XCTAssertEqual(window.panel.frame.height, Metric.field + 1 + Metric.message, "No Results moves up")
        try await until { window.stale }
        window.show()
        defer { window.hide() }
        try await until { window.rows.map(\.name) == ["budget.pdf"] }

        let before = checks
        access = false
        window.recheckAccess()
        XCTAssertNil(window.notice)
        XCTAssertEqual(checks, before, "with access there is nothing more to check")
    }

    func testTheListSavedAtTheLastLaunchAnswersAtOnce() async throws {
        try Data().write(to: root.appendingPathComponent("budget.pdf"))
        let support = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: support) }
        let file = support.appendingPathComponent("index").path
        do {
            let first = try await opened(access: true, saved: file)
            // Saved 2 s after the first list.
            for _ in 0..<1000 where !FileManager.default.fileExists(atPath: file) {
                try await Task.sleep(for: .milliseconds(10))
            }
            XCTAssertTrue(FileManager.default.fileExists(atPath: file))
            XCTAssertFalse(first.reopened)
        }
        // Gone before `support` is, so no save is under way as the folder goes.
        do {
            let window = try await opened(access: true, saved: file)
            XCTAssertTrue(window.reopened)
            try await type("budget", in: window)
            try await until { window.rows.map(\.name) == ["budget.pdf"] }
        }
    }

    func testANewListKeepsTheSelectedFileSelected() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        down(window)
        let picked = try XCTUnwrap(window.selected?.path)
        XCTAssertEqual(window.rows.firstIndex { $0.path == picked }, 1)

        // A shorter name ranks first, pushing the picked file down a row.
        try Data().write(to: notes.appendingPathComponent("budget.txt"))
        try await until { window.rows.count == 3 }
        XCTAssertEqual(window.selected?.path, picked)
        XCTAssertNotEqual(window.rows.firstIndex { $0.path == picked }, 1)
        try await settled(window)
        assertOnScreen(window)
    }

    func testAFileGoneFromTheListMovesTheSelectionToTheTop() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt", "budget c.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        down(window)
        let picked = try XCTUnwrap(window.selected?.path)

        try FileManager.default.removeItem(atPath: picked)
        try await until { window.rows.count == 2 }
        XCTAssertEqual(window.selected, window.rows.first)
        try await settled(window)
        assertOnScreen(window)
    }

    func testAnArrowPressedBeforeTheSearchRunsAgainCounts() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        try Data().write(to: notes.appendingPathComponent("budget c.txt"))
        try await until { window.stale }

        // Shown, the search runs again; its answer can only arrive after the arrow.
        window.show()
        defer { window.hide() }
        down(window)
        let picked = try XCTUnwrap(window.selected?.path)
        XCTAssertEqual(window.rows.firstIndex { $0.path == picked }, 1)
        try await until { window.rows.count == 3 }
        XCTAssertEqual(window.selected?.path, picked)
    }

    func testAHiddenWindowRunsTheSearchAgainWhenShown() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        try Data().write(to: notes.appendingPathComponent("budget a.txt"))
        let window = try await opened(access: true)
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }

        try Data().write(to: notes.appendingPathComponent("budget b.txt"))
        try await until { window.stale }
        XCTAssertEqual(window.rows.count, 1, "nothing runs while hidden")
        window.show()
        defer { window.hide() }
        XCTAssertFalse(window.stale)
        try await until { window.rows.count == 2 }
        try await settled(window)
        assertOnScreen(window)
    }

    func testANewListRewritesTheRowsOnScreenInPlace() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt", "report.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        for name in ["budget c", "plans"] {
            try FileManager.default.createDirectory(
                at: notes.appendingPathComponent(name), withIntermediateDirectories: false)
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        try await settled(window)
        assertOnScreen(window)
        let top = try XCTUnwrap(window.table.rowView(atRow: 0, makeIfNecessary: false))

        try await type("report", in: window)
        try await until { window.rows.map(\.name) == ["report.txt"] }
        try await settled(window)
        XCTAssertEqual(window.table.numberOfRows, 1)
        XCTAssertTrue(window.table.rowView(atRow: 0, makeIfNecessary: false) === top, "the row is kept")
        assertOnScreen(window)
        let page = try XCTUnwrap(cell(window, 0))
        let text = try XCTUnwrap(page.icon.image)

        try await type("plans", in: window)
        try await until { window.rows.map(\.name) == ["plans"] }
        try await settled(window)
        XCTAssertTrue(cell(window, 0) === page)
        XCTAssertFalse(page.icon.image === text, "a folder in the row a file was in gets the folder icon")
        assertOnScreen(window)

        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        try await settled(window)
        assertOnScreen(window)

        try await type("", in: window)
        XCTAssertEqual(window.table.numberOfRows, 0)
        var left = 0
        window.table.enumerateAvailableRowViews { _, _ in left += 1 }
        XCTAssertEqual(left, 0)
        try await until { window.panel.frame.height == Metric.field }

        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        try await settled(window)
        assertOnScreen(window)
    }

    func testARenamedFileOnScreenShowsItsNewName() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        try await settled(window)
        let views = (0..<2).map { window.table.rowView(atRow: $0, makeIfNecessary: false) }

        try FileManager.default.moveItem(
            at: notes.appendingPathComponent("budget a.txt"), to: notes.appendingPathComponent("budget z.txt"))
        try await until { Set(window.rows.map(\.name)) == ["budget b.txt", "budget z.txt"] }
        try await settled(window)
        assertOnScreen(window)
        for row in 0..<2 {
            XCTAssertTrue(window.table.rowView(atRow: row, makeIfNecessary: false) === views[row])
        }
    }

    func testRowsScrolledIntoViewShowTheNewList() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for number in 1...12 {
            for word in ["budget", "report"] {
                try Data().write(to: notes.appendingPathComponent("\(word) \(number).txt"))
            }
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 12 }
        try await settled(window)
        window.table.scrollRowToVisible(11)
        window.panel.displayIfNeeded()
        XCTAssertTrue(window.table.rows(in: window.table.visibleRect).contains(11))
        assertOnScreen(window)

        try await type("report", in: window)
        try await until { window.rows.count == 12 && window.rows.allSatisfy { $0.name.hasPrefix("report") } }
        try await settled(window)
        assertOnScreen(window)
        window.table.scrollRowToVisible(11)
        window.panel.displayIfNeeded()
        XCTAssertTrue(window.table.rows(in: window.table.visibleRect).contains(11))
        assertOnScreen(window)
    }

    func testRowIconsLookLikeFindersOwn() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        try Data().write(to: notes.appendingPathComponent("budget.pdf"))
        try FileManager.default.createDirectory(
            at: notes.appendingPathComponent("budget"), withIntermediateDirectories: false)
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        try await settled(window)
        for row in 0..<2 {
            let drawn = try XCTUnwrap(cell(window, row)?.icon.image)
            let finder = NSWorkspace.shared.icon(for: window.rows[row].isFolder ? .folder : .pdf)
            for scale in [1, 2] {
                for look in [NSAppearance.Name.aqua, .darkAqua] {
                    let appearance = try XCTUnwrap(NSAppearance(named: look))
                    _ = pixels(of: finder, scale: scale, in: appearance)
                    let mine = try XCTUnwrap(pixels(of: drawn, scale: scale, in: appearance))
                    let theirs = try XCTUnwrap(pixels(of: finder, scale: scale, in: appearance))
                    let label = "\(window.rows[row].name) at \(scale)x, \(look.rawValue)"
                    XCTAssertGreaterThan(mine.filter { $0 != 0 }.count, mine.count / 4, "\(label) is blank")
                    XCTAssertEqual(zip(mine, theirs).filter { $0 != $1 }.count, 0, "\(label) differs")
                }
            }
        }
    }

    func testOneIconPerKindWhateverTheCase() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["plan a.pdf", "PLAN B.PDF", "plan c", "plan d.zzqq"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("plan", in: window)
        try await until { window.rows.count == 4 }
        try await settled(window)
        assertOnScreen(window)
        func icon(_ name: String) throws -> NSImage {
            let row = try XCTUnwrap(window.rows.firstIndex { $0.name == name })
            return try XCTUnwrap(cell(window, row)?.icon.image)
        }
        XCTAssertTrue(try icon("plan a.pdf") === icon("PLAN B.PDF"))
        for name in ["plan c", "plan d.zzqq"] {
            let pixels = try XCTUnwrap(pixels(of: icon(name), scale: 2, in: NSAppearance(named: .aqua)!))
            XCTAssertGreaterThan(pixels.filter { $0 != 0 }.count, pixels.count / 4, "\(name) is blank")
        }
    }

    func testARowIconCanBeDrawnOffTheMainThread() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        try Data().write(to: notes.appendingPathComponent("budget.pdf"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        try await settled(window)
        let drawn = try XCTUnwrap(cell(window, 0)?.icon.image)
        // A size the row never drew, so the copy draws again, here off the main thread.
        let inked = await Task.detached { Self.inked(drawn, side: 96) }.value
        XCTAssertGreaterThan(inked, 96 * 96)
    }

    func testAnAppShowsWithItsOwnIcon() async throws {
        let window = try await opened(access: true, apps: ["/System/Applications"])
        window.show()
        defer { window.hide() }
        let aqua = try XCTUnwrap(NSAppearance(named: .aqua))
        let generic = try XCTUnwrap(pixels(of: NSWorkspace.shared.icon(for: .applicationBundle), scale: 2, in: aqua))
        var seen: [String: NSImage] = [:]
        for name in ["Calculator", "Chess", "Calculator"] {
            try await type(name.lowercased(), in: window)
            try await until { window.rows.first?.name == name }
            try await settled(window)
            let row = try XCTUnwrap(window.rows.first)
            XCTAssertTrue(row.isApp)
            XCTAssertFalse(row.isFolder)
            XCTAssertEqual(row.path, "/System/Applications/\(name).app")
            assertOnScreen(window)
            if let before = seen[name] {
                XCTAssertTrue(before === cell(window, 0)?.icon.image, "\(name)'s icon is drawn once")
                continue
            }
            let drawn = try await ownIcon(window, row: 0)
            let mine = try XCTUnwrap(pixels(of: drawn, scale: 2, in: aqua))
            XCTAssertGreaterThan(mine.filter { $0 != 0 }.count, mine.count / 4, "\(name) is blank")
            XCTAssertNotEqual(mine, generic, "\(name)'s own icon, not the one every app shares")
            XCTAssertFalse(seen.values.contains { $0 === drawn })
            seen[name] = drawn
        }
        let calculator = try XCTUnwrap(pixels(of: XCTUnwrap(seen["Calculator"]), scale: 2, in: aqua))
        XCTAssertNotEqual(calculator, try pixels(of: XCTUnwrap(seen["Chess"]), scale: 2, in: aqua))
    }

    /// /Applications/Safari.app is a link like this one; read at the link, the icon was a document's.
    func testAnAppReachedThroughALinkShowsTheAppsIcon() async throws {
        let apps = realRoot.appendingPathComponent("Applications")
        let elsewhere = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: elsewhere) }
        for folder in [apps.appendingPathComponent("Budget Plain.app"), elsewhere.appendingPathComponent("Budget Linked.app")] {
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        }
        try FileManager.default.createSymbolicLink(
            at: apps.appendingPathComponent("Budget Linked.app"),
            withDestinationURL: elsewhere.appendingPathComponent("Budget Linked.app"))
        let window = try await opened(access: true, apps: [apps.path])
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.filter(\.isApp).count == 2 }
        try await settled(window)
        let aqua = try XCTUnwrap(NSAppearance(named: .aqua))
        var drawn: [[UInt8]] = []
        for row in window.rows.indices where window.rows[row].isApp {
            let icon = try await ownIcon(window, row: row)
            drawn.append(try XCTUnwrap(pixels(of: icon, scale: 2, in: aqua)))
        }
        XCTAssertTrue(drawn[0] == drawn[1], "both are plain app folders, so both show the plain app icon")
        for kind in [UTType.folder, .data] {
            XCTAssertNotEqual(drawn[0], pixels(of: NSWorkspace.shared.icon(for: kind), scale: 2, in: aqua), "\(kind)")
        }
    }

    /// Read at the link, the pane said "Alias – 54 bytes" for Safari.
    func testThePaneDescribesAnAppReachedThroughALinkAsTheApp() async throws {
        let apps = realRoot.appendingPathComponent("Applications")
        let elsewhere = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: elsewhere) }
        let real = elsewhere.appendingPathComponent("Budget Linked.app")
        try FileManager.default.createDirectory(at: apps, withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: real, withIntermediateDirectories: true)
        try FileManager.default.createSymbolicLink(
            at: apps.appendingPathComponent("Budget Linked.app"), withDestinationURL: real)
        let window = try await opened(access: true, apps: [apps.path])
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.first?.isApp == true }
        XCTAssertEqual(window.selected?.path, apps.appendingPathComponent("Budget Linked.app").path)
        window.toggleDetails(nil)
        XCTAssertEqual(window.pane.details?.type, .applicationBundle)
        XCTAssertFalse(window.pane.summary.stringValue.contains("–"), window.pane.summary.stringValue)
        let resolved = real.resolvingSymlinksInPath().path
        XCTAssertEqual(window.pane.waiting?.path, resolved)
        XCTAssertEqual(item(window)?.path, resolved)
        XCTAssertEqual(window.pane.place.stringValue, apps.path, "found in the folder it is listed in")
    }

    func testAnAppIconArrivingAfterTheRowsChangedStaysOffThem() async throws {
        try Data().write(to: root.appendingPathComponent("budget.pdf"))
        let window = try await opened(access: true, apps: ["/System/Applications"])
        window.show()
        defer { window.hide() }
        try await type("calculator", in: window)
        try await until { window.rows.first?.name == "Calculator" }
        window.panel.displayIfNeeded()
        // Typed on before Quick Look is likely done.
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["budget.pdf"] }
        try await until { window.readingIcons.isEmpty }
        try await settled(window)
        assertOnScreen(window)
        let icon = try XCTUnwrap(cell(window, 0)?.icon.image)
        XCTAssertNotEqual(icon.representations.first?.pixelsWide, 32 * Int(window.panel.backingScaleFactor))
    }

    /// Waits for an app row's own icon: one picture at the row's size, a few KB, never macOS's
    /// whole icon, which first stands in as the icon every app shares.
    private func ownIcon(_ window: SearchPanel, row: Int) async throws -> NSImage {
        let side = 32 * Int(window.panel.backingScaleFactor)
        try await until { cell(window, row)?.icon.image?.representations.first?.pixelsWide == side }
        let drawn = try XCTUnwrap(cell(window, row)?.icon.image)
        XCTAssertEqual(drawn.size, NSSize(width: 32, height: 32))
        XCTAssertEqual(drawn.representations.count, 1)
        XCTAssertEqual(drawn.representations.first?.pixelsHigh, side)
        return drawn
    }

    func testReturnLaunchesAnAppAndItRanksFirstFromTheNextSearch() async throws {
        // By its real path, as the list holds it, so its bundles show once, as apps.
        let apps = realRoot.appendingPathComponent("Applications")
        for name in ["Budget.app", "Budget Planner.app"] {
            try FileManager.default.createDirectory(
                at: apps.appendingPathComponent(name), withIntermediateDirectories: true)
        }
        let window = try await opened(access: true, apps: [apps.path])
        window.show()
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["Budget", "Budget Planner"] }
        XCTAssertTrue(window.rows.allSatisfy(\.isApp), "\(window.rows)")
        down(window)
        XCTAssertTrue(tab(window), "an app is not a folder to search in")
        XCTAssertTrue(window.scopes.isEmpty)
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertNewline(_:))))
        XCTAssertEqual(stage.opened.map(\.path), [apps.appendingPathComponent("Budget Planner.app").path])
        XCTAssertTrue(try XCTUnwrap(stage.opened.first).hasDirectoryPath)
        XCTAssertFalse(window.shown)

        // The hotkey brings the same search back, already in the new order.
        try await until { window.stale }
        window.show()
        defer { window.hide() }
        try await until { window.rows.map(\.name) == ["Budget Planner", "Budget"] }
        XCTAssertEqual(window.selected?.name, "Budget Planner", "the app opened stays picked")
    }

    func testShowInFinderOnAnAppRevealsItsBundleAndCountsAsAnOpen() async throws {
        let apps = realRoot.appendingPathComponent("Applications")
        for name in ["Budget.app", "Budget Planner.app"] {
            try FileManager.default.createDirectory(
                at: apps.appendingPathComponent(name), withIntermediateDirectories: true)
        }
        let window = try await opened(access: true, apps: [apps.path])
        window.show()
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["Budget", "Budget Planner"] }
        down(window)
        window.showInFinder(nil)
        XCTAssertEqual(stage.shownInFinder.map(\.path), [apps.appendingPathComponent("Budget Planner.app").path])
        XCTAssertTrue(try XCTUnwrap(stage.shownInFinder.first).hasDirectoryPath)
        XCTAssertEqual(stage.opened, [])

        try await until { window.stale }
        window.show()
        defer { window.hide() }
        try await until { window.rows.map(\.name) == ["Budget Planner", "Budget"] }
    }

    func testAnAppInstalledOrDeletedWhileHiddenShowsOnTheNextShow() async throws {
        // Outside your folders, as /Applications is, so no new list runs the search again.
        let outside = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: outside, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: outside) }
        let apps = URL(fileURLWithPath: String(cString: realpath(outside.path, nil)), isDirectory: true)
        let budget = apps.appendingPathComponent("Budget.app")
        let window = try await opened(access: true, apps: [apps.path])
        window.show()
        try await type("budget", in: window)
        try await until { window.answered != 0 }
        XCTAssertEqual(window.rows, [])
        window.hide()

        try FileManager.default.createDirectory(at: budget, withIntermediateDirectories: false)
        window.show()
        try await until { window.rows.map(\.path) == [budget.path] }
        window.hide()

        try FileManager.default.removeItem(at: budget)
        window.show()
        defer { window.hide() }
        try await until { window.rows.isEmpty }
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertNewline(_:))))
        XCTAssertEqual(stage.opened, [])
    }

    /// Argus runs for weeks, through app updates and apps found while still being copied in.
    func testAnAppChangedSinceItsIconWasDrawnIsDrawnAgain() async throws {
        let apps = realRoot.appendingPathComponent("Applications")
        let bundle = apps.appendingPathComponent("Budget.app")
        try FileManager.default.createDirectory(at: bundle, withIntermediateDirectories: true)
        let window = try await opened(access: true, apps: [apps.path])
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.first?.isApp == true }
        try await settled(window)
        let first = try await ownIcon(window, row: 0)
        var answered = window.answered
        try await type("budge", in: window)
        try await until { window.answered != answered }
        try await settled(window)
        XCTAssertTrue(window.readingIcons.isEmpty)
        XCTAssertTrue(cell(window, 0)?.icon.image === first, "drawn once while the app stays as it is")

        let updated = Date(timeIntervalSinceNow: -3600)
        try FileManager.default.setAttributes([.modificationDate: updated], ofItemAtPath: bundle.path)
        answered = window.answered
        try await type("budget", in: window)
        try await until { window.answered != answered }
        try await settled(window)
        let second = try await ownIcon(window, row: 0)
        XCTAssertFalse(second === first, "drawn again for the app as it is now")
    }

    func testWithoutAppFoldersNoAppIsFound() async throws {
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("calculator", in: window)
        try await until { window.answered != 0 }
        XCTAssertEqual(window.rows, [])
    }

    func testWhatYouOpenOrShowInFinderRanksFirstAndIsKeptForTheNextLaunch() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt", "budget c.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let support = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: support) }
        let file = support.appendingPathComponent("opened").path
        let window = try await opened(access: true, opens: file)
        window.show()
        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        let first = window.rows.map(\.name)
        let picked = window.rows[2].path
        down(window)
        down(window)
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertNewline(_:))))
        XCTAssertEqual(stage.opened.map(\.path), [picked])

        try await until { window.stale }
        window.show()
        try await until { window.rows.map(\.name) == [first[2], first[0], first[1]] }
        // The last row, below a file never picked.
        window.table.selectRowIndexes([2], byExtendingSelection: false)
        let revealed = first[1]
        window.showInFinder(nil)
        XCTAssertEqual(stage.shownInFinder.map(\.lastPathComponent), [revealed])
        XCTAssertEqual(stage.opened.count, 1)

        try await until { window.stale }
        window.show()
        // One opened, one shown in Finder: both rank above the file never picked.
        try await until { Set(window.rows.prefix(2).map(\.name)) == [first[2], revealed] }
        XCTAssertEqual(window.rows.last?.name, first[0])
        window.hide()

        // Saved in the order picked, so the second one there means both are.
        let saved = { String(decoding: (try? Data(contentsOf: URL(fileURLWithPath: file))) ?? Data(), as: UTF8.self) }
        try await until { saved().contains(revealed) }
        XCTAssertTrue(saved().contains(first[2]))
        let next = try await opened(access: true, opens: file)
        next.show()
        defer { next.hide() }
        try await type("budget", in: next)
        try await until { Set(next.rows.prefix(2).map(\.name)) == [first[2], revealed] }
    }

    func testTheMadeUpNamesNeverShowTheLine() async throws {
        let window = try await opened(access: false, standIn: 0)
        window.recheckAccess()
        XCTAssertNil(window.notice)
        XCTAssertEqual(checks, 0)
        XCTAssertEqual(window.panel.frame.height, Metric.field)
    }

    func testTabReachesOpenSettingsBothWays() async throws {
        let window = try await opened(access: false)
        for key in [#selector(NSResponder.insertTab(_:)), #selector(NSResponder.insertBacktab(_:))] {
            window.panel.makeFirstResponder(nil)
            XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: key))
            XCTAssertTrue(window.panel.firstResponder === window.settings)
        }
    }

    func testTabWithoutAButtonKeepsTheKeysInTheBox() async throws {
        let granted = try await opened(access: false)
        access = true
        granted.recheckAccess()
        XCTAssertTrue(granted.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertTab(_:))))

        let full = try await opened(access: true)
        XCTAssertTrue(full.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertBacktab(_:))))
    }

    func testTabOnAFolderSearchesInsideItAndBackspaceStepsBackOut() async throws {
        for path in ["notes/budget a.txt", "notes/deep/budget d.txt", "work/budget w.txt"] {
            let file = root.appendingPathComponent(path)
            try FileManager.default.createDirectory(
                at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data().write(to: file)
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        XCTAssertTrue(window.chip.isHidden)
        let wide = window.field.frame

        try await type("notes", in: window)
        try await until { window.rows.first?.name == "notes" }
        XCTAssertTrue(tab(window))
        XCTAssertEqual(window.scopes.map(\.name), ["notes"])
        XCTAssertEqual(window.field.stringValue, "")
        XCTAssertFalse(window.chip.isHidden)
        XCTAssertEqual(window.chipText.stringValue, "notes")
        XCTAssertEqual(window.field.placeholderString, "Search in notes")
        XCTAssertGreaterThan(window.field.frame.minX, window.chip.frame.maxX)
        XCTAssertEqual(window.field.frame.maxX, wide.maxX)
        XCTAssertGreaterThanOrEqual(window.chipText.frame.width, window.chipText.cell!.cellSize.width, "not cut short")
        // An empty box shows no list, once the window has shrunk back.
        try await until { window.panel.frame.height == Metric.field }

        try await type("budget", in: window)
        try await until { Set(window.rows.map(\.name)) == ["budget a.txt", "budget d.txt"] }
        XCTAssertFalse(backspace(window), "with text in the box, Backspace deletes a letter")

        try await type("deep", in: window)
        try await until { window.rows.map(\.name) == ["deep"] }
        XCTAssertTrue(tab(window))
        XCTAssertEqual(window.chipText.stringValue, "notes › deep")
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["budget d.txt"] }

        try await type("", in: window)
        XCTAssertTrue(backspace(window))
        XCTAssertEqual(window.scopes.map(\.name), ["notes"])
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }

        try await type("", in: window)
        XCTAssertTrue(backspace(window))
        XCTAssertTrue(window.scopes.isEmpty)
        XCTAssertTrue(window.chip.isHidden)
        XCTAssertEqual(window.field.frame, wide)
        XCTAssertEqual(window.field.placeholderString, "Search")
        XCTAssertFalse(backspace(window), "nothing left to step out of")
        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
    }

    func testTabOnAFileOrNoResultsKeepsTheKeysInTheBox() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        XCTAssertTrue(tab(window), "an empty box")
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        XCTAssertTrue(tab(window))
        XCTAssertEqual(window.field.stringValue, "budget")
        try await type("zzzzqqq", in: window)
        try await until { window.rows.isEmpty && window.panel.frame.height > Metric.field }
        XCTAssertTrue(tab(window))
        XCTAssertTrue(window.scopes.isEmpty)
    }

    func testTabOnAFileStillReachesOpenSettings() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.panel.makeFirstResponder(nil)
        XCTAssertTrue(tab(window))
        XCTAssertTrue(window.panel.firstResponder === window.settings)
        XCTAssertTrue(window.scopes.isEmpty)
    }

    func testTabPressedBeforeTheResultsArriveWaitsForThem() async throws {
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("notes/inner"), withIntermediateDirectories: true)
        let window = try await opened(access: true)
        try await type("notes", in: window)
        XCTAssertTrue(tab(window), "Tab waits for the results for what you typed")
        try await until { window.scopes.map(\.name) == ["notes"] }
        try await type("inner", in: window)
        try await until { window.rows.map(\.name) == ["inner"] }

        // Typing on cancels a Tab still waiting.
        try await type("", in: window)
        XCTAssertTrue(backspace(window))
        try await type("note", in: window)
        XCTAssertTrue(tab(window))
        try await type("notes", in: window)
        try await until { window.rows.first?.name == "notes" }
        XCTAssertTrue(window.scopes.isEmpty)
    }

    func testInsideAFolderTheSearchRunsAgainOnANewList() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        try Data().write(to: notes.appendingPathComponent("budget a.txt"))
        try Data().write(to: root.appendingPathComponent("budget top.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("notes", in: window)
        try await until { window.rows.first?.name == "notes" }
        XCTAssertTrue(tab(window))
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["budget a.txt"] }

        try Data().write(to: notes.appendingPathComponent("budget b.txt"))
        try await until { window.rows.count == 2 }
        XCTAssertEqual(Set(window.rows.map(\.name)), ["budget a.txt", "budget b.txt"])

        try FileManager.default.removeItem(at: notes)
        try await until { window.rows.isEmpty }
        XCTAssertEqual(window.scopes.map(\.name), ["notes"], "a folder gone keeps its chip until you leave it")
    }

    func testTheChipKeepsLongNamesInsideItsWidth() async throws {
        let long = String(repeating: "quarterly report ", count: 6).trimmingCharacters(in: .whitespaces)
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent(long), withIntermediateDirectories: false)
        let window = try await opened(access: true)
        try await type("quarterly", in: window)
        try await until { window.rows.first?.name == long }
        XCTAssertTrue(tab(window))
        XCTAssertLessThanOrEqual(window.chip.frame.width, Metric.chip)
        XCTAssertGreaterThan(window.field.frame.width, Metric.width / 2)
        XCTAssertEqual(window.chip.toolTip, window.scopes.last?.path)
    }

    func testTheTextChipSitsBeforeTheFolderChipAndBackspaceLeavesOnlyTheFolder() async throws {
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("notes"), withIntermediateDirectories: false)
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("notes", in: window)
        try await until { window.rows.first?.name == "notes" }
        XCTAssertTrue(tab(window))
        window.toggleText(nil)
        XCTAssertFalse(window.chip.isHidden)
        XCTAssertFalse(window.textChip.isHidden)
        XCTAssertEqual(window.textChip.frame.minX, 50)
        XCTAssertGreaterThan(window.chip.frame.minX, window.textChip.frame.maxX)
        XCTAssertGreaterThan(window.field.frame.minX, window.chip.frame.maxX)
        XCTAssertGreaterThanOrEqual(
            window.textChipText.frame.width, window.textChipText.cell!.cellSize.width, "not cut short")

        XCTAssertTrue(backspace(window))
        XCTAssertTrue(window.scopes.isEmpty)
        XCTAssertTrue(window.textOn, "⌘T alone turns it off")
        XCTAssertGreaterThan(window.field.frame.minX, window.textChip.frame.maxX)
    }

    func testTabPressedBeforeAFileArrivesStillReachesOpenSettings() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        try await type("budget", in: window)
        window.panel.makeFirstResponder(nil)
        XCTAssertTrue(tab(window), "Tab waits for the results for what you typed")
        try await until { window.panel.firstResponder === window.settings }
        XCTAssertTrue(window.scopes.isEmpty)
    }

    func testTabEntersTheFolderYouPicked() async throws {
        for name in ["plan a.txt", "plan b.txt"] {
            try Data().write(to: root.appendingPathComponent(name))
        }
        for name in ["plan c", "plan d"] {
            try FileManager.default.createDirectory(
                at: root.appendingPathComponent(name), withIntermediateDirectories: false)
        }
        let window = try await opened(access: true)
        try await type("plan", in: window)
        try await until { window.rows.count == 4 }
        let move = { (row: Int) in
            while let at = window.rows.firstIndex(where: { $0.path == window.selected?.path }), at != row {
                let way = at < row ? #selector(NSResponder.moveDown(_:)) : #selector(NSResponder.moveUp(_:))
                XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: way))
            }
        }
        move(try XCTUnwrap(window.rows.lastIndex { !$0.isFolder }))
        XCTAssertTrue(tab(window), "a file")
        XCTAssertTrue(window.scopes.isEmpty)
        let folder = try XCTUnwrap(window.rows.lastIndex { $0.isFolder })
        XCTAssertGreaterThan(folder, 0)
        move(folder)
        let picked = window.rows[folder]
        XCTAssertTrue(tab(window))
        XCTAssertEqual(window.scopes, [picked])
    }

    func testHoldingBackspaceStopsAtTheEmptyBox() async throws {
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("notes/inner"), withIntermediateDirectories: true)
        let window = try await opened(access: true)
        for name in ["notes", "inner"] {
            try await type(name, in: window)
            try await until { window.rows.first?.name == name }
            XCTAssertTrue(tab(window))
        }
        defer { clearCurrentEvent() }
        try currentKey(repeating: true)
        XCTAssertTrue(backspace(window), "a held key is used up")
        XCTAssertEqual(window.scopes.map(\.name), ["notes", "inner"])
        try currentKey(repeating: false)
        XCTAssertTrue(backspace(window))
        XCTAssertEqual(window.scopes.map(\.name), ["notes"])
    }

    func testHalfTypedJapaneseKeepsTabAndBackspaceForTheInputMethod() async throws {
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("notes"), withIntermediateDirectories: false)
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("notes", in: window)
        try await until { window.rows.first?.name == "notes" }
        window.panel.makeFirstResponder(window.field)
        let editor = try XCTUnwrap(window.field.currentEditor() as? NSTextView)
        let nowhere = NSRange(location: NSNotFound, length: 0)
        editor.setMarkedText("か", selectedRange: NSRange(location: 1, length: 0), replacementRange: nowhere)
        XCTAssertFalse(tab(window))
        XCTAssertTrue(window.scopes.isEmpty)

        editor.unmarkText()
        try await type("notes", in: window)
        XCTAssertTrue(tab(window))
        try await until { window.scopes.map(\.name) == ["notes"] }
        editor.setMarkedText("か", selectedRange: NSRange(location: 1, length: 0), replacementRange: nowhere)
        XCTAssertTrue(editor.hasMarkedText())
        XCTAssertFalse(backspace(window))
        XCTAssertEqual(window.scopes.map(\.name), ["notes"])
    }

    func testAFolderWithAccentsAndJapaneseIsSearchedInside() async throws {
        let folder = root.appendingPathComponent("notes é/会議")
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        try Data().write(to: folder.appendingPathComponent("budget.txt"))
        try Data().write(to: root.appendingPathComponent("budget top.txt"))
        let window = try await opened(access: true)
        try await type("会議", in: window)
        try await until { window.rows.first?.name == "会議" }
        XCTAssertTrue(tab(window))
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["budget.txt"] }
    }

    func testTheFolderIsStillSearchedAfterHidingAndShowing() async throws {
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("notes"), withIntermediateDirectories: false)
        let window = try await opened(access: true)
        window.show()
        try await type("notes", in: window)
        try await until { window.rows.first?.name == "notes" }
        XCTAssertTrue(tab(window))
        window.hide()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.scopes.map(\.name), ["notes"], "the window comes back as you left it")
        XCTAssertFalse(window.chip.isHidden)
    }

    func testTheFinderHotkeyOpensTheWindowInsideFindersFolder() async throws {
        for path in ["notes/budget a.txt", "budget top.txt"] {
            let file = root.appendingPathComponent(path)
            try FileManager.default.createDirectory(
                at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data().write(to: file)
        }
        let notes = realRoot.appendingPathComponent("notes").path
        let window = try await opened(access: true, finder: { .folder(notes) })
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }

        window.toggleInFinderFolder()
        XCTAssertFalse(window.shown, "waits for Finder's answer")
        try await until { window.shown }
        XCTAssertEqual(window.scopes.map(\.path), [notes])
        XCTAssertEqual(window.chipText.stringValue, "notes")
        XCTAssertEqual(window.field.stringValue, "", "a new folder starts a new search")
        XCTAssertTrue(stage.key === window.panel)
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["budget a.txt"] }

        window.toggleInFinderFolder()
        XCTAssertFalse(window.shown, "pressed again, it hides")
    }

    func testWithoutAFolderInFinderTheHotkeyOpensTheWindowAsYouLeftIt() async throws {
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("notes"), withIntermediateDirectories: false)
        for answer in [FinderFolder.none, .failed(Int(errAETimeout))] {
            let window = try await opened(access: true, finder: { answer })
            try await type("notes", in: window)
            try await until { window.rows.first?.name == "notes" }
            XCTAssertTrue(tab(window))
            window.toggleInFinderFolder()
            try await until { window.shown }
            defer { window.hide() }
            XCTAssertEqual(window.scopes.map(\.name), ["notes"], "\(answer)")
            XCTAssertNil(window.notice)
        }
    }

    func testFinderSwitchedOffShowsTheLineToItsOwnSettingsPage() async throws {
        for access in [true, false] {
            let stage = OffStage()
            let window = try await opened(access: access, on: stage, finder: { .denied })
            let before = window.panel.frame.height
            window.toggleInFinderFolder()
            try await until { window.shown }
            XCTAssertEqual(window.notice, .needsFinder)
            XCTAssertFalse(window.settings.isHidden)
            XCTAssertEqual(window.panel.frame.height, Metric.field + 1 + Metric.message)
            assertFits(window)
            XCTAssertTrue(window.scopes.isEmpty)

            window.settings.performClick(nil)
            XCTAssertEqual(stage.opened, [Notice.needsFinder.settings])
            XCTAssertFalse(window.shown)
            // Gone with the window; the access line, if any, comes back.
            try await until { window.notice == (access ? nil : .needsAccess) }
            XCTAssertEqual(window.panel.frame.height, before)
            window.show()
            defer { window.hide() }
            XCTAssertEqual(window.notice, access ? nil : .needsAccess)
        }
    }

    func testFullDiskAccessGrantedWhileTheFinderLineShowsStillCounts() async throws {
        let window = try await opened(access: false, finder: { .denied })
        window.toggleInFinderFolder()
        try await until { window.shown }
        access = true
        window.recheckAccess()
        XCTAssertFalse(window.lacksAccess)
        XCTAssertEqual(window.notice, .needsFinder, "the Finder line stays while the window does")
        window.hide()
        try await until { window.notice == nil }
    }

    func testOnlyTheNewestFinderAnswerOpensTheWindow() async throws {
        for name in ["first", "second"] {
            try FileManager.default.createDirectory(
                at: root.appendingPathComponent(name), withIntermediateDirectories: false)
        }
        let (first, second) = (realRoot.appendingPathComponent("first").path, realRoot.appendingPathComponent("second").path)
        let asked = Asked()
        let window = try await opened(access: true) {
            if asked.next() == 1 {
                Thread.sleep(forTimeInterval: 0.3)
                return .folder(first)
            }
            return .folder(second)
        }
        window.toggleInFinderFolder()
        window.toggleInFinderFolder()
        try await until { window.shown }
        defer { window.hide() }
        XCTAssertEqual(window.scopes.map(\.path), [second])
        try await Task.sleep(for: .milliseconds(500))
        XCTAssertEqual(window.scopes.map(\.path), [second], "the late answer is dropped")
    }

    /// macOS's question can keep Finder's answer waiting while you search the usual way.
    func testAFinderAnswerAfterTheWindowOpenedAnotherWayIsDropped() async throws {
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("notes"), withIntermediateDirectories: false)
        let notes = realRoot.appendingPathComponent("notes").path
        let gate = Gate()
        let window = try await opened(access: true) {
            gate.wait()
            return .folder(notes)
        }
        for hidden in [false, true] {
            window.toggleInFinderFolder()
            window.show()
            try await type("budget", in: window)
            if hidden { window.hide() }
            gate.open()
            try await until { gate.answered }
            // The answer reaches the window on its next turn.
            try await Task.sleep(for: .milliseconds(100))
            XCTAssertEqual(window.shown, !hidden, "\(hidden)")
            XCTAssertTrue(window.scopes.isEmpty, "\(hidden)")
            XCTAssertEqual(window.field.stringValue, "budget", "what you typed stays")
            window.hide()
            gate.reset()
        }
    }

    func testAFolderTheListLeavesOutOpensTheWindowAsYouLeftIt() async throws {
        for path in ["Downloads/budget.txt", "notes/budget.txt"] {
            let file = root.appendingPathComponent(path)
            try FileManager.default.createDirectory(
                at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data().write(to: file)
        }
        let downloads = realRoot.appendingPathComponent("Downloads").path
        for (access, folder) in [(false, downloads), (true, "/"), (true, NSTemporaryDirectory())] {
            let window = try await opened(access: access, finder: { .folder(folder) })
            window.toggleInFinderFolder()
            try await until { window.shown }
            defer { window.hide() }
            XCTAssertTrue(window.scopes.isEmpty, folder)
            XCTAssertTrue(window.chip.isHidden)
            XCTAssertEqual(window.notice, access ? nil : .needsAccess, "the line says why Downloads is left out")
            try await type("budget", in: window)
            try await until { !window.rows.isEmpty }
            XCTAssertTrue(window.rows.contains { $0.path.hasSuffix("notes/budget.txt") }, folder)
        }
    }

    /// Hidden and shown again within the fade, by the usual hotkey.
    func testTheFinderLineGoesWithTheFinderWindowEvenMidFade() async throws {
        for access in [true, false] {
            let window = try await opened(access: access, finder: { .denied })
            window.toggleInFinderFolder()
            try await until { window.shown }
            XCTAssertEqual(window.notice, .needsFinder)
            window.hide()
            window.toggle()
            defer { window.hide() }
            XCTAssertTrue(window.shown)
            XCTAssertEqual(window.notice, access ? nil : .needsAccess)
            XCTAssertEqual(window.settings.isHidden, access)
            assertFits(window)
            try await Task.sleep(for: .milliseconds(Int(Metric.fade * 1_000) + 100))
            XCTAssertEqual(window.notice, access ? nil : .needsAccess, "the fade's end changes nothing")
        }
    }

    func testCommandWHidesTheWindowAsEscDoes() async throws {
        let window = try await opened(access: true)
        window.show()
        let close = NSMenuItem(title: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        XCTAssertTrue(window.panel.validateMenuItem(close), "not greyed out, so ⌘W doesn't beep")
        window.panel.performClose(nil)
        XCTAssertFalse(window.shown)
    }

    /// The health view's own chain ends at the app's delegate, which shows the view again.
    func testCommandCommaIsHeardWhileTheHealthViewIsKey() async throws {
        let window = try await opened(access: true)
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        defer { health.hide() }
        let action = #selector(SearchPanel.showHealth(_:))
        XCTAssertFalse(health.responds(to: action))
        XCTAssertTrue(AppDelegate().responds(to: action))
    }

    func testCommandCommaSwapsTheSearchWindowForTheHealthView() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        window.show()
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        defer { health.hide() }
        XCTAssertFalse(window.shown)
        XCTAssertTrue(health.shown)
        XCTAssertTrue(stage.key === health.panel)
        XCTAssertEqual(health.entries.stringValue, "2", "the folder and the budget")
        XCTAssertTrue(health.access.stringValue.hasPrefix("Off"))
        XCTAssertFalse(health.grid.row(at: 5).isHidden)
        XCTAssertNotEqual(health.scanned.stringValue, "Under way")

        // Switched on while the view shows: the next tick brings in the guarded folders.
        access = true
        try await until { health.access.stringValue == "On" }
        XCTAssertFalse(window.lacksAccess)
        XCTAssertNil(window.notice)
        try await until { health.health.changedAt != nil && !health.health.busy }
        XCTAssertEqual(health.state.stringValue, "Up to date")
    }

    func testTheHealthViewsRebuildBuildsTheListAgain() async throws {
        let window = try await opened(access: true)
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        defer { health.hide() }
        XCTAssertNil(health.health.changedAt)
        health.rebuild.performClick(nil)
        try await until { health.health.changedAt != nil && !health.health.busy }
        XCTAssertTrue(health.rebuild.isEnabled)
        XCTAssertNotNil(health.health.scannedAt)
        window.showHealth(nil)
        XCTAssertTrue(window.health === health, "one view, shown again")
    }

    func testTheCachedHealthViewShowsTheSwitchAsItIsNow() async throws {
        let everything = IndexEverything()
        let window = try await opened(access: true, everything: everything)
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        health.hide()
        everything.isOn = true
        window.showHealth(nil)
        defer { health.hide() }
        XCTAssertTrue(window.health === health)
        XCTAssertEqual(health.everything.state, .on)
    }

    func testTheHealthViewsSwitchListsCachesLiveAndIsKept() async throws {
        let caches = root.appendingPathComponent("Library/Caches")
        try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
        try Data().write(to: caches.appendingPathComponent("cached.db"))
        let defaults = MemoryDefaults()
        let window = try await opened(access: true, everything: IndexEverything(defaults: defaults))
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        defer { health.hide() }
        XCTAssertEqual(health.everything.state, .off)
        XCTAssertEqual(health.entries.stringValue, "2", "the folder and Library")

        health.everything.performClick(nil)
        XCTAssertTrue(IndexEverything(defaults: defaults).isOn, "kept for the next launch")
        // The folder, Library, Caches and cached.db.
        try await until { health.health.entries == 4 && !health.health.busy }
        XCTAssertTrue(health.progress.isHidden)

        health.everything.performClick(nil)
        XCTAssertFalse(IndexEverything(defaults: defaults).isOn)
        try await until { health.health.entries == 2 && !health.health.busy }
    }

    func testTheSwitchKeptOnListsCachesFromLaunch() async throws {
        let caches = root.appendingPathComponent("Library/Caches")
        try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
        try Data().write(to: caches.appendingPathComponent("cached.db"))
        let everything = IndexEverything()
        everything.isOn = true
        let window = try await opened(access: true, everything: everything)
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        defer { health.hide() }
        XCTAssertEqual(health.everything.state, .on)
        XCTAssertEqual(health.entries.stringValue, "4")
    }

    func testTheSwitchWaitsForFullDiskAccessThenListsCaches() async throws {
        let caches = root.appendingPathComponent("Library/Caches")
        try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
        try Data().write(to: caches.appendingPathComponent("cached.db"))
        let everything = IndexEverything()
        everything.isOn = true
        let window = try await opened(access: false, everything: everything)
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        defer { health.hide() }
        XCTAssertEqual(health.entries.stringValue, "2", "the folder and Library, without the caches")
        XCTAssertTrue(health.everythingHint.stringValue.hasSuffix("Needs Full Disk Access."))

        access = true
        try await until { health.health.entries == 4 && !health.health.busy }
        XCTAssertTrue(health.everything.isEnabled)
    }

    func testASwitchTurnedOffWithoutAccessStaysOffWhenAccessComes() async throws {
        let caches = root.appendingPathComponent("Library/Caches")
        try FileManager.default.createDirectory(at: caches, withIntermediateDirectories: true)
        try Data().write(to: caches.appendingPathComponent("cached.db"))
        let everything = IndexEverything()
        everything.isOn = true
        let window = try await opened(access: false, everything: everything)
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        defer { health.hide() }
        health.everything.performClick(nil)
        XCTAssertFalse(everything.isOn)

        access = true
        try await until { health.health.changedAt != nil && !health.health.busy }
        XCTAssertEqual(health.health.entries, 2, "the list access brought skips the caches")
    }

    func testARowsURLIsItsPathAndKnowsAFolder() {
        let file = Row(name: "budget.txt", folder: "/Users/me/notes", isFolder: false)
        XCTAssertEqual(file.url, URL(fileURLWithPath: "/Users/me/notes/budget.txt"))
        XCTAssertTrue(file.url.isFileURL)
        XCTAssertFalse(file.url.hasDirectoryPath)

        let folder = Row(name: "notes", folder: "/Users/me", isFolder: true)
        XCTAssertEqual(folder.url.path, "/Users/me/notes")
        XCTAssertTrue(folder.url.hasDirectoryPath)

        // The searched folder is listed under its full path.
        let top = Row(name: "/Users/me", folder: "", isFolder: true)
        XCTAssertEqual(top.url.path, "/Users/me")
        XCTAssertTrue(top.url.hasDirectoryPath)

        let disk = Row(name: "/", folder: "", isFolder: true)
        XCTAssertEqual(disk.url.path, "/")
    }

    func testNamesURLsTreatAsSpecialKeepTheirPath() {
        for name in ["100% budget #2?.txt", "budget%20plain.txt", "café 会議.md", "a;b=c&d.txt", ".hidden", "~tilde"] {
            let row = Row(name: name, folder: "/Users/me/odd folder", isFolder: false)
            XCTAssertEqual(row.url.path, row.path, name)
            XCTAssertEqual(row.url.lastPathComponent, name)
        }
    }

    func testFilesOnDiskComeBackFromTheirURL() async throws {
        let names = ["100% budget #2?.txt", "budget café 会議.md", "budget%20plain.txt"]
        for name in names {
            try Data().write(to: root.appendingPathComponent(name))
        }
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("budget folder"), withIntermediateDirectories: false)
        let window = try await opened(access: true)
        try await type("budget", in: window)
        try await until { window.rows.count == names.count + 1 }
        for row in window.rows {
            var folder: ObjCBool = false
            XCTAssertTrue(FileManager.default.fileExists(atPath: row.url.path, isDirectory: &folder), row.name)
            XCTAssertEqual(folder.boolValue, row.url.hasDirectoryPath, row.name)
        }
    }

    func testNothingSelectedHasNothingToPreview() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        let preview = QLPreviewPanel.shared()!
        XCTAssertEqual(window.numberOfPreviewItems(in: preview), 0)
        XCTAssertNil(window.previewPanel(preview, previewItemAt: 0))
        window.quickLook(nil)
        XCTAssertFalse(window.previewing)
        XCTAssertNil(window.missing)

        // The same window previews once something is picked.
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
    }

    func testThePreviewIsTheSelectedRowAndFollowsTheArrows() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        let preview = QLPreviewPanel.shared()!
        window.panel.beginPreviewPanelControl(preview)
        defer { window.panel.endPreviewPanelControl(preview) }
        XCTAssertTrue(window.panel.acceptsPreviewPanelControl(preview))
        XCTAssertTrue(preview.dataSource === window)
        XCTAssertTrue(preview.delegate as AnyObject === window)
        XCTAssertEqual(preview.level, window.panel.level, "in front of the window, not under it")
        XCTAssertEqual(preview.collectionBehavior, window.panel.collectionBehavior, "over full-screen apps too")

        XCTAssertEqual(window.numberOfPreviewItems(in: preview), 1)
        XCTAssertEqual(item(window), window.rows[0].url)
        down(window)
        XCTAssertEqual(item(window), window.rows[1].url)
        down(window)
        XCTAssertEqual(item(window), window.rows[1].url, "the last row stays picked")

        // The arrows keep working while the preview has the keys; other keys are its own.
        XCTAssertTrue(window.previewPanel(preview, handle: key("\u{F700}")))
        XCTAssertEqual(item(window), window.rows[0].url)
        XCTAssertTrue(window.previewPanel(preview, handle: key("\u{F701}")))
        XCTAssertEqual(item(window), window.rows[1].url)
        XCTAssertFalse(window.previewPanel(preview, handle: key(" ")))
        XCTAssertFalse(window.previewPanel(preview, handle: key("\u{1B}")))
        XCTAssertEqual(item(window), window.rows[1].url)

        // A key-up would move the row twice; ⌘, ⌥ and ⌃ arrows scroll the preview itself.
        XCTAssertFalse(window.previewPanel(preview, handle: key("\u{F700}", type: .keyUp)))
        for flag in [NSEvent.ModifierFlags.command, .option, .control] {
            XCTAssertFalse(window.previewPanel(preview, handle: key("\u{F700}", flags: [flag, .function, .numericPad])))
        }
        XCTAssertEqual(item(window), window.rows[1].url)
        XCTAssertTrue(window.previewPanel(preview, handle: key("\u{F700}", flags: [.function, .numericPad])))
        XCTAssertEqual(item(window), window.rows[0].url)
    }

    func testEndingControlLetsGoOfThePreview() async throws {
        let window = try await opened(access: true)
        let preview = QLPreviewPanel.shared()!
        window.panel.beginPreviewPanelControl(preview)
        window.panel.endPreviewPanelControl(preview)
        XCTAssertNil(preview.dataSource)
        XCTAssertNil(preview.delegate)
    }

    func testShowingBringsTheWindowForwardWithTheKeysInTheBox() async throws {
        let window = try await opened(access: true)
        XCTAssertEqual(stage.forward, 0)
        window.show()
        defer { window.hide() }
        XCTAssertEqual(stage.forward, 1)
        XCTAssertFalse(window.panel.isVisible, "the test keeps it off the screen")
        XCTAssertTrue(stage.isKey(window.panel))
        XCTAssertTrue(window.panel.firstResponder === window.field.currentEditor())
        XCTAssertFalse(NSApp.isActive, "the window takes the keys without making Argus the active app")
    }

    func testHidingPutsTheWindowAwayWithItsKeys() async throws {
        let window = try await opened(access: true)
        window.show()
        window.hide()
        try await until { stage.away == 1 }
        XCTAssertFalse(stage.isKey(window.panel))
    }

    /// While you search, the core leaves its slow tidy-up of the list for once the window hides.
    func testShowingAndHidingTellsTheCore() async throws {
        let window = try await opened(access: true)
        let hatch = try XCTUnwrap(window.hatch)
        XCTAssertFalse(hatch.windowShown(false), "the core starts out knowing the window is hidden")
        window.show()
        XCTAssertFalse(hatch.windowShown(true), "told on show")
        window.hide()
        XCTAssertFalse(hatch.windowShown(false), "told on hide")
        window.show()
        defer { window.hide() }
        XCTAssertFalse(hatch.windowShown(true), "told on each show")
    }

    func testShownAgainWhileFadingOutTheWindowStays() async throws {
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        window.hide()
        window.show()
        try await Task.sleep(for: .seconds(Metric.fade + 0.2))
        XCTAssertEqual(stage.away, 0)
        XCTAssertTrue(stage.isKey(window.panel))
    }

    func testWarmingUpLeavesTheWindowAwayWithoutTheKeys() async throws {
        let window = try await opened(access: true)
        window.warmUp()
        XCTAssertEqual(stage.forward, 1)
        XCTAssertEqual(stage.away, 1)
        XCTAssertNil(stage.key)
        XCTAssertFalse(window.shown)
        XCTAssertEqual(window.panel.alphaValue, 0)
    }

    func testAPreviewClosedWhileArgusIsActiveHandsTheKeysBackToTheBox() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        window.panel.makeFirstResponder(window.settings)

        // A click on the preview makes Argus the active app and gives the preview the keys.
        stage.isActive = true
        stage.key = nil
        stage.previewIsKey = true
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: window.panel))
        await nextTurn()
        XCTAssertTrue(window.shown, "a click on the preview keeps the window")
        XCTAssertNil(stage.key, "the preview keeps the keys")

        // Esc on the preview: once it has faded out, Quick Look lets go of the window, and
        // nothing else has picked up the keys.
        window.closePreview()
        stage.finishFade()
        window.panel.endPreviewPanelControl(QLPreviewPanel.shared())
        await nextTurn()
        XCTAssertTrue(window.shown)
        XCTAssertTrue(stage.isKey(window.panel))
        XCTAssertTrue(window.panel.firstResponder === window.field.currentEditor())
    }

    func testAPreviewClosedWhileAnotherAppIsActiveHidesTheWindow() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        stage.key = nil
        window.closePreview()
        stage.finishFade()
        window.panel.endPreviewPanelControl(QLPreviewPanel.shared())
        await nextTurn()
        XCTAssertFalse(window.shown)
        XCTAssertNil(stage.key)
        try await until { stage.away == 1 }
    }

    func testOnScreenAPreviewClosedWhileArgusIsActiveHandsTheKeysBackToTheBox() async throws {
        try onScreenOnly()
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false, on: Screen())
        window.show()
        defer { window.hide() }
        let unlocked = await soon { window.panel.isKeyWindow }
        try XCTSkipUnless(unlocked, "needs an unlocked screen")
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        let preview = QLPreviewPanel.shared()!
        try await until { preview.currentController as AnyObject === window.panel }

        // A click on the preview makes Argus the active app and gives the preview the keys.
        NSApp.activate(ignoringOtherApps: true)
        try await until { NSApp.isActive }
        window.panel.makeFirstResponder(window.settings)
        preview.makeKey()
        try await until { preview.isKeyWindow }
        await nextTurn()
        XCTAssertTrue(window.shown, "a click on the preview keeps the window")

        // Esc on the preview.
        window.closePreview()
        try await until { window.panel.isKeyWindow }
        try await until { window.panel.firstResponder === window.field.currentEditor() }
        XCTAssertTrue(window.shown)
    }

    func testAPreviewClosedWithTheWindowAlreadyKeyLeavesTheFocusWhereItWas() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        window.panel.makeFirstResponder(window.settings)

        // macOS handed the keys back to the window on its own.
        stage.isActive = true
        window.closePreview()
        stage.finishFade()
        window.panel.endPreviewPanelControl(QLPreviewPanel.shared())
        await nextTurn()
        XCTAssertTrue(window.shown)
        XCTAssertTrue(window.panel.firstResponder === window.settings)
    }

    func testFocusLeavingArgusWithAPreviewUpHidesBoth() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        // A click in another app: no window of Argus's has the keys, and it isn't active.
        stage.key = nil
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: QLPreviewPanel.shared()))
        XCTAssertTrue(window.shown, "decided once macOS has picked the next key window")
        try await until { !window.shown }
        try await until { !window.previewing }
        XCTAssertEqual(stage.closes, 1)
    }

    func testTheSearchWindowLosingTheKeysWithAPreviewUpWaitsToDecide() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        // A click on the preview takes the keys from the search window first.
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: window.panel))
        XCTAssertTrue(window.shown)
        XCTAssertTrue(window.previewing)
        stage.key = nil
        stage.previewIsKey = true
        await nextTurn()
        XCTAssertTrue(window.shown, "the preview has the keys by the time it decides")
        XCTAssertTrue(window.previewing)
        XCTAssertNil(stage.key)
    }

    func testAPreviewOpeningWhileAnotherAppIsActiveHandsTheKeysBackToTheBox() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        // Quick Look takes the keys as it comes up, with Argus still not the active app.
        stage.key = nil
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: window.panel))
        window.windowDidBecomeKey(Notification(name: NSWindow.didBecomeKeyNotification, object: QLPreviewPanel.shared()))
        await nextTurn()
        XCTAssertTrue(window.shown)
        XCTAssertTrue(window.previewing)
        XCTAssertTrue(stage.isKey(window.panel))
        XCTAssertTrue(window.panel.firstResponder === window.field.currentEditor())
    }

    func testOnScreenAPreviewOpeningKeepsTheWindow() async throws {
        try onScreenOnly()
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true, on: Screen())
        window.show()
        defer { window.hide() }
        let unlocked = await soon { window.panel.isKeyWindow }
        try XCTSkipUnless(unlocked, "needs an unlocked screen")
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        // Quick Look comes up over about 0.3 s and settles the keys at the end.
        try await Task.sleep(for: .seconds(1))
        XCTAssertTrue(window.shown)
        XCTAssertTrue(window.previewing)
    }

    func testFocusLeavingTheWindowWithoutAPreviewHidesItAtOnce() async throws {
        let window = try await opened(access: true)
        window.show()
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: window.panel))
        XCTAssertFalse(window.shown)
    }

    /// Hiding with the preview up ends its control, which settles the keys a turn later.
    func testAHiddenWindowIgnoresTheKeysSettling() async throws {
        let window = try await opened(access: true)
        window.show()
        window.hide()
        try await until { stage.away == 1 }
        stage.isActive = true
        window.panel.makeFirstResponder(window.settings)
        window.panel.endPreviewPanelControl(QLPreviewPanel.shared())
        await nextTurn()
        XCTAssertFalse(window.shown)
        XCTAssertTrue(window.panel.firstResponder === window.settings, "the box isn't handed the keys")
        XCTAssertNil(stage.key, "nor is the window")
    }

    func testEscOnOpenSettingsClosesThePreviewFirst() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        window.panel.makeFirstResponder(window.settings)

        window.panel.cancelOperation(nil)
        try await until { !window.previewing }
        XCTAssertEqual(stage.closes, 1)
        XCTAssertTrue(window.shown)
        window.panel.cancelOperation(nil)
        XCTAssertFalse(window.shown)
    }

    func testCommandYPressedBeforeTheResultsArriveOpensTheNewTopRow() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "report.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("report", in: window)
        try await until { window.rows.count == 1 }

        try await type("budget", in: window)
        window.quickLook(nil)
        XCTAssertFalse(window.previewing, "waits for the results for what was typed")
        try await until { window.previewing }
        XCTAssertEqual(window.rows.map(\.name), ["budget a.txt"])
        XCTAssertEqual(window.inPreview, window.rows[0].url)
    }

    func testCommandYThenHidingBeforeTheResultsArriveOpensNothing() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        try await type("budget", in: window)
        window.quickLook(nil)
        window.hide()
        try await until { window.rows.count == 1 }
        await nextTurn()
        XCTAssertEqual(stage.opens, 0)
        XCTAssertFalse(stage.previewShows)
    }

    func testCommandYClosesThePreviewEvenWhileResultsAreOnTheirWay() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        window.quickLook(nil)
        try await until { window.previewing }

        // Typing on right after must not drop the close.
        try await type("budget ", in: window)
        window.quickLook(nil)
        try await type("budget", in: window)
        try await until { !window.previewing }
        try await until { window.rows.count == 2 }
        await nextTurn()
        XCTAssertFalse(window.previewing, "nothing opens it again")
        XCTAssertEqual(stage.closes, 1)
        XCTAssertEqual(stage.opens, 1)
    }

    func testHalfTypedJapaneseOpensNoPreview() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.panel.makeFirstResponder(window.field)
        let editor = try XCTUnwrap(window.field.currentEditor() as? NSTextView)
        editor.setMarkedText("か", selectedRange: NSRange(location: 1, length: 0), replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertTrue(editor.hasMarkedText())
        XCTAssertNotNil(window.selected)

        window.quickLook(nil)
        await nextTurn()
        XCTAssertFalse(window.previewing)
        XCTAssertNil(window.missing)
    }

    func testAClickOnARowMovesThePreview() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt", "budget c.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        window.quickLook(nil)
        try await until { window.previewing }
        XCTAssertEqual(window.inPreview, window.rows[0].url)

        window.table.selectRowIndexes([2], byExtendingSelection: false)
        XCTAssertEqual(window.inPreview, window.rows[2].url)
        down(window)
        XCTAssertEqual(window.inPreview, window.rows[2].url, "the last row stays picked")
        XCTAssertTrue(window.previewing)
        XCTAssertEqual(stage.reloads, 1, "only a new file reloads the preview")
        XCTAssertEqual(stage.opens, 1)
    }

    func testClearingTheBoxClosesThePreview() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        // Clearing asks to close from both the empty list and the cleared selection.
        try await type("", in: window)
        XCTAssertFalse(window.previewing)
        stage.finishFade()
        XCTAssertFalse(stage.previewShows)
        XCTAssertEqual(stage.closes, 1, "a second close would keep it open")
        XCTAssertTrue(window.shown)
    }

    func testEscTwiceQuicklyClosesThePreviewThenClearsTheBox() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        // The second Esc lands while the preview is still fading out.
        let esc = #selector(NSResponder.cancelOperation(_:))
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: esc))
        XCTAssertTrue(stage.previewShows)
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: esc))
        XCTAssertEqual(window.field.stringValue, "")
        stage.finishFade()
        await nextTurn()
        XCTAssertFalse(stage.previewShows, "stays closed")
        XCTAssertEqual(stage.closes, 1)
        XCTAssertTrue(window.shown)
    }

    func testCommandYWhileThePreviewFadesOutOpensItAgain() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        window.quickLook(nil)
        XCTAssertTrue(stage.fading)
        window.quickLook(nil)
        XCTAssertTrue(window.previewing)
        stage.finishFade()
        XCTAssertTrue(stage.previewShows)
        XCTAssertEqual(stage.opens, 2)
        XCTAssertEqual(stage.closes, 1)
    }

    func testANewListKeepsPreviewingThePickedFile() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        window.quickLook(nil)
        try await until { window.previewing }
        down(window)
        let picked = try XCTUnwrap(window.selected?.url)
        XCTAssertEqual(window.inPreview, picked)

        // A shorter name ranks first and moves the picked file down a row.
        try Data().write(to: notes.appendingPathComponent("budget.txt"))
        try await until { window.rows.count == 3 }
        XCTAssertEqual(window.selected?.url, picked)
        XCTAssertEqual(window.inPreview, picked)
        XCTAssertTrue(window.previewing)
        XCTAssertEqual(stage.reloads, 1, "the same file isn't loaded again")
    }

    func testReturnOnAFileGoneWhilePreviewingClosesThePreviewAndSaysSo() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        window.quickLook(nil)
        try await until { window.previewing }
        let row = try XCTUnwrap(window.selected)

        try FileManager.default.removeItem(atPath: row.path)
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertNewline(_:))))
        XCTAssertEqual(window.missing, "“\(row.name)” can’t be found.")
        try await until { !window.previewing }
        XCTAssertEqual(stage.closes, 1)
        XCTAssertTrue(window.shown)

        down(window)
        XCTAssertNil(window.missing, "the first arrow brings the list back")
        XCTAssertEqual(window.selected, row, "on the same row")
    }

    func testAMadeUpNameSaysItCantBeFoundInsteadOfPreviewing() async throws {
        let window = try await opened(access: true, standIn: 1000)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { !window.rows.isEmpty }
        let row = try XCTUnwrap(window.selected)
        window.quickLook(nil)
        XCTAssertFalse(window.previewing)
        XCTAssertEqual(window.missing, "“\(row.name)” can’t be found.")
        XCTAssertTrue(window.shown)

        down(window)
        XCTAssertNil(window.missing, "the first arrow brings the list back")
        XCTAssertEqual(window.selected, row, "on the same row")
    }

    func testEscAndCommandYCloseThePreviewButKeepTheSearch() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }

        window.quickLook(nil)
        try await until { window.previewing }
        XCTAssertTrue(window.shown)

        // Esc in the box closes the preview first and keeps the search.
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.cancelOperation(_:))))
        try await until { !window.previewing }
        XCTAssertEqual(stage.closes, 1)
        XCTAssertEqual(window.field.stringValue, "budget")
        XCTAssertTrue(window.shown)

        stage.finishFade()
        window.quickLook(nil)
        try await until { window.previewing }
        window.quickLook(nil)
        try await until { !window.previewing }
        XCTAssertEqual(stage.closes, 2)
        XCTAssertTrue(window.shown, "⌘Y again closes only the preview")
    }

    func testOnScreenQuickLookFindsTheWindowAndShowsTheSelectedFile() async throws {
        try onScreenOnly()
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true, on: Screen())
        window.show()
        defer { window.hide() }
        let unlocked = await soon { window.panel.isKeyWindow }
        try XCTSkipUnless(unlocked, "needs an unlocked screen")
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        window.quickLook(nil)
        let preview = QLPreviewPanel.shared()!
        try await until { preview.currentController as AnyObject === window.panel }
        XCTAssertTrue(window.panel.isKeyWindow, "typing and the arrows stay in the box")
        try await until { previewed() == window.rows[0].url }
        down(window)
        try await until { previewed() == window.rows[1].url }
    }

    func testThePreviewClosesWhenNothingIsLeftToShow() async throws {
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        try await type("zzzz", in: window)
        try await until { !window.previewing }
        XCTAssertTrue(window.rows.isEmpty)
        XCTAssertEqual(stage.closes, 1)

        stage.finishFade()
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        window.hide()
        try await until { !window.previewing }
        XCTAssertEqual(stage.closes, 2)
    }

    func testAFileGoneWhileShownClosesThePreviewQuietly() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        window.quickLook(nil)
        try await until { window.previewing }

        XCTAssertEqual(window.inPreview, window.rows[0].url)

        // Gone before the list catches up, so the next row is still listed. The list stays, as it
        // does without a preview; ⌘Y on that row is what says it can't be found.
        try FileManager.default.removeItem(atPath: window.rows[1].path)
        down(window)
        try await until { !window.previewing }
        XCTAssertEqual(stage.closes, 1)
        XCTAssertNil(window.missing)
        XCTAssertEqual(window.selected, window.rows[1])
        window.quickLook(nil)
        XCTAssertEqual(window.missing, "“\(window.rows[1].name)” can’t be found.")
        XCTAssertFalse(window.previewing)
    }

    func testCommandIOpensThePaneBesideThePickedFileAndClosesIt() async throws {
        try Data("twelve bytes".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        try await settled(window)
        XCTAssertEqual(window.panel.frame.width, Metric.width)
        let left = window.panel.frame.minX
        let top = window.panel.frame.maxY

        window.toggleDetails(nil)
        XCTAssertTrue(window.detailing)
        XCTAssertFalse(window.pane.isHidden)
        try await until { window.panel.frame.width == Metric.width + 1 + Metric.pane }
        try await until { window.panel.frame.height == Metric.field + 1 + Metric.details }
        XCTAssertEqual(window.panel.frame.minX, left, "grows to the right")
        XCTAssertEqual(window.panel.frame.maxY, top, "the box stays put")
        XCTAssertEqual(window.pane.frame.maxX, window.panel.contentView!.bounds.width)
        XCTAssertEqual(window.pane.name.stringValue, "budget.txt")
        let values = try window.rows[0].url.resourceValues(forKeys: [
            .localizedTypeDescriptionKey, .creationDateKey, .contentModificationDateKey,
        ])
        let format = DetailsFormat()
        XCTAssertEqual(window.pane.summary.stringValue, "\(values.localizedTypeDescription!) – 12 bytes")
        XCTAssertEqual(window.pane.place.stringValue, window.rows[0].folder)
        XCTAssertEqual(window.pane.modified.stringValue, format.date(values.contentModificationDate))
        XCTAssertEqual(window.pane.created.stringValue, format.date(values.creationDate))

        window.toggleDetails(nil)
        XCTAssertFalse(window.detailing)
        XCTAssertTrue(window.pane.isHidden)
        try await until { window.panel.frame.width == Metric.width }
        try await settled(window)
        XCTAssertEqual(window.panel.frame.minX, left)
        XCTAssertNil(window.pane.waiting, "an unfinished thumbnail is given up")
    }

    func testThePaneFollowsTheArrowsAndAClick() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt", "budget c.txt"] {
            try Data(name.utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        window.toggleDetails(nil)
        XCTAssertEqual(window.pane.row, window.rows[0])
        down(window)
        XCTAssertEqual(window.pane.row, window.rows[1])
        XCTAssertEqual(window.pane.name.stringValue, window.rows[1].name)
        window.table.selectRowIndexes([2], byExtendingSelection: false)
        XCTAssertEqual(window.pane.row, window.rows[2])
        XCTAssertEqual(window.pane.waiting, window.rows[2].url, "only the picked file's thumbnail is drawn")
    }

    func testAThumbnailReplacesTheIconAndAnOlderOneNeverLands() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\nnumbers\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        window.toggleDetails(nil)
        XCTAssertEqual(window.pane.waiting, window.rows[0].url)
        let icon = window.pane.picture.image
        XCTAssertNotNil(icon, "the icon shows at once")
        down(window)
        XCTAssertNil(window.pane.thumbnailed)
        XCTAssertEqual(window.pane.waiting, window.rows[1].url)

        try await until { window.pane.thumbnailed != nil }
        XCTAssertEqual(window.pane.thumbnailed, window.rows[1].url)
        XCTAssertNil(window.pane.waiting)
        XCTAssertFalse(window.pane.picture.image === icon)
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(window.pane.thumbnailed, window.rows[1].url, "the first file's thumbnail never lands")
    }

    func testThePaneStepsAsideWhileNothingIsPicked() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        window.toggleDetails(nil)
        XCTAssertFalse(window.detailing, "an empty box has nothing to show")
        XCTAssertEqual(window.panel.frame.width, Metric.width)

        try await type("budget", in: window)
        try await until { window.detailing }
        try await until { window.panel.frame.width == Metric.width + 1 + Metric.pane }

        try await type("zzzz", in: window)
        try await until { window.rows.isEmpty }
        XCTAssertFalse(window.detailing, "No Results")
        XCTAssertTrue(window.pane.isHidden)
        try await until { window.panel.frame.width == Metric.width }
        XCTAssertEqual(window.panel.frame.height, Metric.field + 1 + Metric.message)

        try await type("budget", in: window)
        try await until { window.detailing }
        XCTAssertTrue(window.detailsOn, "the switch stays on")
        try await until { window.panel.frame.width == Metric.width + 1 + Metric.pane }
    }

    func testThePaneStaysOnAcrossHidesAndShowsWhatChangedMeanwhile() async throws {
        let url = root.appendingPathComponent("budget.txt")
        try Data("abc".utf8).write(to: url)
        let window = try await opened(access: true)
        window.show()
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.toggleDetails(nil)
        XCTAssertTrue(window.pane.summary.stringValue.hasSuffix("3 bytes"), window.pane.summary.stringValue)
        window.hide()

        try Data("abcdef".utf8).write(to: url)
        window.show()
        defer { window.hide() }
        XCTAssertTrue(window.detailing)
        XCTAssertEqual(window.panel.frame.width, Metric.width + 1 + Metric.pane, "opens at its full width")
        XCTAssertTrue(window.pane.summary.stringValue.hasSuffix("6 bytes"), window.pane.summary.stringValue)
    }

    func testAMadeUpNameShowsItCantBeFoundWithoutAThumbnail() async throws {
        let window = try await opened(access: true, standIn: 1000)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { !window.rows.isEmpty }
        window.toggleDetails(nil)
        XCTAssertTrue(window.detailing)
        XCTAssertTrue(window.pane.summary.stringValue.hasSuffix("can’t be found"), window.pane.summary.stringValue)
        XCTAssertEqual(window.pane.created.stringValue, "—")
        XCTAssertNil(window.pane.waiting)
        XCTAssertNotNil(window.pane.picture.image)
    }

    func testWithThePaneClosedTheArrowsReadNothing() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        down(window)
        XCTAssertNil(window.pane.row)
        XCTAssertNil(window.pane.waiting)
    }

    /// The top row stays row 0, so the table says nothing; the pane must still follow the file.
    func testANewListWithAnotherTopFileShowsThatFile() async throws {
        try Data("abc".utf8).write(to: root.appendingPathComponent("budget.txt"))
        try Data("abcdef".utf8).write(to: root.appendingPathComponent("budget plan.md"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 2 }
        window.toggleDetails(nil)
        XCTAssertEqual(window.pane.row, window.rows[0])

        try await type("budget p", in: window)
        try await until { window.rows.map(\.name) == ["budget plan.md"] }
        XCTAssertEqual(window.table.selectedRow, 0)
        XCTAssertEqual(window.pane.row, window.rows[0])
        XCTAssertEqual(window.pane.name.stringValue, "budget plan.md")
        XCTAssertTrue(window.pane.summary.stringValue.hasSuffix("6 bytes"), window.pane.summary.stringValue)
        XCTAssertEqual(window.pane.waiting, window.rows[0].url)
    }

    func testAFileSavedWhileThePaneShowsGetsItsNewSize() async throws {
        let url = root.appendingPathComponent("budget.txt")
        try Data("abc".utf8).write(to: url)
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.toggleDetails(nil)
        let picked = window.rows[0].url
        try await until { window.pane.thumbnailed == picked }

        // The new file ranks below, so the saved one keeps row 0 in the new list.
        try Data("abcdef".utf8).write(to: url)
        try Data().write(to: root.appendingPathComponent("budget 2.txt"))
        try await until { window.rows.count == 2 }
        XCTAssertEqual(window.selected?.url, picked)
        XCTAssertEqual(window.table.selectedRow, 0)
        XCTAssertTrue(window.pane.summary.stringValue.hasSuffix("6 bytes"), window.pane.summary.stringValue)
        XCTAssertEqual(window.pane.waiting, picked, "a new thumbnail for the new contents")
    }

    func testAClickBelowTheRowsKeepsThePick() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.toggleDetails(nil)
        let kept = window.tableView(window.table, selectionIndexesForProposedSelection: [])
        XCTAssertEqual(kept, [0])
        XCTAssertEqual(window.tableView(window.table, selectionIndexesForProposedSelection: [0]), [0])
    }

    func testThePaneMovesTheWindowOffTheRightEdgeAndClosingMovesItBack() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        try await settled(window)
        let visible = try XCTUnwrap(window.panel.screen?.visibleFrame)
        // Dragged close to the right edge.
        let left = visible.maxX - Metric.width - 10
        window.panel.setFrameOrigin(NSPoint(x: left, y: window.panel.frame.minY))
        let top = window.panel.frame.maxY

        window.toggleDetails(nil)
        try await until { window.panel.frame.width == Metric.width + 1 + Metric.pane }
        try await settled(window)
        XCTAssertEqual(window.panel.frame.maxX, visible.maxX, accuracy: 0.5, "stays on the screen")
        XCTAssertEqual(window.panel.frame.maxY, top)

        window.toggleDetails(nil)
        try await until { window.panel.frame.width == Metric.width }
        try await settled(window)
        XCTAssertEqual(window.panel.frame.minX, left, accuracy: 0.5, "the box goes back where it was")
    }

    func testAListTallerThanThePaneSetsTheHeight() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for index in 1...7 {
            try Data().write(to: notes.appendingPathComponent("budget \(index).txt"))
        }
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 7 }
        window.toggleDetails(nil)
        let list = 7 * Metric.row + 2 * Metric.inset
        XCTAssertGreaterThan(list, Metric.details)
        try await until { window.panel.frame.height == Metric.field + 1 + list }
        XCTAssertEqual(window.pane.frame.height, list)
    }

    func testWithoutAccessTheButtonStaysAtTheRightEndBesideThePane() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        let gap = Metric.width - window.settings.frame.maxX
        window.toggleDetails(nil)
        try await until { window.panel.frame.width == Metric.width + 1 + Metric.pane }
        try await settled(window)
        let bar = try XCTUnwrap(window.settings.superview)
        XCTAssertEqual(bar.bounds.width, window.panel.frame.width)
        XCTAssertEqual(bar.bounds.width - window.settings.frame.maxX, gap)
        assertFits(window)
    }

    func testWithNoFolderChosenNothingIsReadInsideFiles() async throws {
        try write("q3 budget review", at: "notes/plan.txt")
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.status == "No Results" }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(window.textTicket, 0)
        XCTAssertEqual(window.hits, [])
        XCTAssertNil(window.header)
    }

    func testLinesInsideFilesListUnderTheNamesAndAfterThem() async throws {
        try write("nothing to see", at: "notes/budget.txt")
        try write("intro\n\tq3 budget review\n", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        XCTAssertEqual(window.textTicket, 0, "no file is read before the names are drawn")
        try await until { window.rows.map(\.name) == ["budget.txt"] && window.hits.count == 1 }
        try await until { window.textTicket == 0 }

        let hit = try XCTUnwrap(window.hits.first)
        XCTAssertEqual(hit.path, realRoot.appendingPathComponent("notes/plan.txt").path)
        XCTAssertEqual(hit.line, 2)
        XCTAssertEqual(hit.text, "q3 budget review")
        XCTAssertEqual((hit.text as NSString).substring(with: hit.match), "budget")
        XCTAssertEqual(window.header, 1)
        XCTAssertEqual(window.table.numberOfRows, 3)
        XCTAssertEqual(window.table.selectedRow, 0, "the names come first and keep the pick")
        XCTAssertNil(window.status)
        try await settled(window)
        XCTAssertEqual(window.table.rect(ofRow: 1).height, Metric.header)
        XCTAssertEqual(window.table.rect(ofRow: 2).height, Metric.row)
        XCTAssertEqual(window.panel.frame.height, Metric.field + 1 + 2 * Metric.row + Metric.header + 2 * Metric.inset)

        let title = try XCTUnwrap(cell(window, 1))
        XCTAssertTrue(title.isHeader)
        XCTAssertEqual(title.name.stringValue, "Inside Files")
        XCTAssertTrue(title.icon.isHidden)
        let line = try XCTUnwrap(cell(window, 2))
        XCTAssertEqual(line.hit, hit)
        XCTAssertEqual(line.name.stringValue, "q3 budget review")
        XCTAssertEqual(line.folder.stringValue, "plan.txt, line 2 · \(realRoot.appendingPathComponent("notes").path)")
        let bold = line.name.attributedStringValue.attribute(.font, at: 3, effectiveRange: nil) as? NSFont
        XCTAssertTrue(bold?.fontDescriptor.symbolicTraits.contains(.bold) ?? false, "the match is bold")
        let plain = line.name.attributedStringValue.attribute(.font, at: 0, effectiveRange: nil) as? NSFont
        XCTAssertFalse(plain?.fontDescriptor.symbolicTraits.contains(.bold) ?? true)
    }

    func testEachKeystrokeStopsTheTextSearchAndItsLinesNeverLand() async throws {
        try writeMany()
        for at in 0..<200 {
            try write("budget line \(at)\nreview line \(at)\n", at: "notes/plan \(at).txt")
        }
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await untilClose { window.textTicket != 0 }
        try await type("review", in: window)
        XCTAssertEqual(window.textTicket, 0, "stopped at the keystroke")
        let matches = { Set(window.hits.map { ($0.text as NSString).substring(with: $0.match) }) }
        try await until { window.textTicket == 0 && window.hits.count == 100 && matches() == ["review"] }
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(window.hits.count, 100, "no line found for the earlier search lands late")
        XCTAssertEqual(matches(), ["review"])
    }

    func testOnlyLinesFoundPicksTheFirstAndReturnOpensItsFile() async throws {
        try write("a zebra crossing", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("zebra", in: window)
        try await until { window.answered != 0 }
        XCTAssertEqual(window.status, window.hits.isEmpty ? "Searching inside files…" : nil)
        try await until { window.hits.count == 1 }
        XCTAssertEqual(window.rows, [])
        XCTAssertEqual(window.table.selectedRow, 1)
        let file = realRoot.appendingPathComponent("notes/plan.txt")
        XCTAssertEqual(window.selected?.path, file.path)
        XCTAssertEqual(
            window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.moveUp(_:))), true)
        XCTAssertEqual(window.table.selectedRow, 1, "the title is never picked")
        XCTAssertTrue(
            window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertNewline(_:))))
        XCTAssertEqual(stage.opened, [file])
        XCTAssertFalse(window.shown)
    }

    func testArrowsAndClicksStepOverTheTitle() async throws {
        for name in ["budget a.txt", "budget b.txt"] { try write("x", at: "notes/\(name)") }
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.rows.count == 2 && window.hits.count == 1 }
        XCTAssertEqual(window.header, 2)
        down(window)
        XCTAssertEqual(window.table.selectedRow, 1)
        down(window)
        XCTAssertEqual(window.table.selectedRow, 3)
        XCTAssertEqual(window.selected?.name, "plan.txt")
        down(window)
        XCTAssertEqual(window.table.selectedRow, 3, "the last line stays picked")
        _ = window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.moveUp(_:)))
        XCTAssertEqual(window.table.selectedRow, 1)
        let clicked = window.tableView(window.table, selectionIndexesForProposedSelection: [2])
        XCTAssertEqual(clicked, [1], "a click on the title keeps the pick")
    }

    func testAPlainSearchReadsNoFile() async throws {
        try write("the budget", at: "notes/plan.txt")
        try write("x", at: "notes/budget.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        XCTAssertFalse(window.textOn)
        XCTAssertTrue(window.textChip.isHidden)
        try await type("budget", in: window)
        try await until { window.rows.map(\.name) == ["budget.txt"] }
        try await type("the budget", in: window)
        try await until { window.answered != 0 && window.rows.isEmpty }
        XCTAssertEqual(window.status, "No Results", "never Searching inside files…")
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(window.textTicket, 0)
        XCTAssertEqual(window.hits, [])
        XCTAssertNil(window.header)
    }

    func testCommandTSearchesTheSameWordsInsideFilesAndBackToNames() async throws {
        try write("the budget", at: "notes/plan.txt")
        try write("x", at: "notes/budget.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        let wide = window.field.frame

        window.toggleText(nil)
        XCTAssertTrue(window.textOn)
        XCTAssertFalse(window.textChip.isHidden)
        XCTAssertEqual(window.textChipText.stringValue, "Inside Files")
        XCTAssertGreaterThan(window.field.frame.minX, window.textChip.frame.maxX)
        XCTAssertEqual(window.field.frame.maxX, wide.maxX)
        XCTAssertEqual(window.field.stringValue, "budget", "the same words")
        XCTAssertNotEqual(window.textTicket, 0, "starts at once: the names are already drawn")
        try await until { window.hits.count == 1 && window.textTicket == 0 }
        XCTAssertEqual(window.rows.map(\.name), ["budget.txt"])
        down(window)
        XCTAssertEqual(window.selected?.name, "plan.txt")

        window.toggleText(nil)
        XCTAssertFalse(window.textOn)
        XCTAssertTrue(window.textChip.isHidden)
        XCTAssertEqual(window.field.frame, wide)
        XCTAssertEqual(window.hits, [])
        XCTAssertNil(window.header)
        XCTAssertEqual(window.table.numberOfRows, 1)
        XCTAssertEqual(window.table.selectedRow, 0, "the picked line hands the pick to the top name")
        try await until { window.panel.frame.height == Metric.field + 1 + Metric.row + 2 * Metric.inset }
    }

    func testCommandTWhileTheNamesAreOnTheirWayWaitsForThem() async throws {
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        window.toggleText(nil)
        try await until { window.answered != 0 && window.hits.count == 1 && window.textTicket == 0 }
    }

    func testCommandTOffMidSearchStopsItAndItsLinesNeverLand() async throws {
        try writeMany()
        for at in 0..<30 { try write("x\nthe budget \(at)\n", at: "filler/\(at)/plan.txt") }
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await untilClose { window.textTicket != 0 }
        window.toggleText(nil)
        XCTAssertEqual(window.textTicket, 0)
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(window.hits, [])
        XCTAssertEqual(window.status, "No Results")
    }

    /// Hiding switches back to names; the lines and the chip go once the window is out of sight.
    func testHidingMidSearchStopsItAndShowingSearchesNamesOnly() async throws {
        try writeMany()
        // Fewer lines than the 100 a search stops at, so it reads every file.
        for at in 0..<30 { try write("x\nthe budget \(at)\n", at: "filler/\(at)/plan.txt") }
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await untilClose { window.textTicket != 0 && !window.hits.isEmpty }
        window.hide()
        XCTAssertEqual(window.textTicket, 0, "hiding stops it")
        XCTAssertFalse(window.textOn)
        XCTAssertFalse(window.textChip.isHidden, "nothing moves while the window fades out")
        try await until { window.hits.isEmpty && window.textChip.isHidden }
        try await settled(window)

        window.show()
        XCTAssertFalse(window.textOn)
        XCTAssertEqual(window.textTicket, 0, "no file is read on show")
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(window.hits, [])

        window.toggleText(nil)
        try await until { window.textTicket == 0 && window.hits.count == 30 }
        XCTAssertEqual(Set(window.hits.map(\.path)).count, 30, "no line listed twice")
    }

    func testShownAgainWhileFadingOutTheLinesAndChipGo() async throws {
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.hits.count == 1 && window.textTicket == 0 }
        down(window)
        window.hide()
        window.show()
        XCTAssertFalse(window.textOn)
        XCTAssertTrue(window.textChip.isHidden)
        XCTAssertEqual(window.hits, [])
        XCTAssertEqual(window.textTicket, 0)
        XCTAssertNil(window.header)
    }

    /// Sent as AppKit sends a key equivalent: the menu finds ⌘T's action through the window.
    func testCommandTIsTheWindowsOwnKeyAndClashesWithNoOther() async throws {
        let items = keyMenu().items.flatMap { $0.submenu?.items ?? [] }
        let bound = items.filter { $0.keyEquivalent == "t" && $0.keyEquivalentModifierMask == .command }
        XCTAssertEqual(bound.map(\.action), [#selector(SearchPanel.toggleText(_:))])
        let keys = items.map { "\($0.keyEquivalentModifierMask.rawValue) \($0.keyEquivalent)" }
        XCTAssertEqual(Set(keys).count, keys.count, "no two items share a key")

        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        window.panel.makeFirstResponder(window.field)
        let editor = try XCTUnwrap(window.field.currentEditor())
        let commandT = key("t", flags: .command)
        XCTAssertFalse(editor.performKeyEquivalent(with: commandT), "the box leaves ⌘T to the menu")
        XCTAssertFalse(window.panel.performKeyEquivalent(with: commandT))
        XCTAssertTrue(window.panel.tryToPerform(try XCTUnwrap(bound.first?.action), with: bound.first))
        XCTAssertTrue(window.textOn)
        XCTAssertTrue(window.panel.tryToPerform(try XCTUnwrap(bound.first?.action), with: bound.first))
        XCTAssertFalse(window.textOn)
    }

    func testClearingTheBoxDropsTheLinesAndTypingAgainFindsThem() async throws {
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.hits.count == 1 && window.textTicket == 0 }
        try await type("", in: window)
        XCTAssertEqual(window.hits, [])
        XCTAssertEqual(window.table.numberOfRows, 0)
        XCTAssertEqual(window.textTicket, 0)
        try await until { window.panel.frame.height == Metric.field }

        try await type("budget", in: window)
        try await until { window.hits.count == 1 && window.textTicket == 0 }
        XCTAssertNil(window.status)
    }

    func testLinesFollowAFileEditedWhileHidden() async throws {
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.hits.map(\.line) == [1] && window.textTicket == 0 }
        window.hide()
        try write("intro\nthe budget", at: "notes/plan.txt")
        window.show()
        window.toggleText(nil)
        try await until { window.hits.map(\.line) == [2] && window.textTicket == 0 }
        XCTAssertEqual(window.table.selectedRow, 1)
    }

    func testAWordNoNameHasSaysSearchingFromTheFirstFrame() async throws {
        try writeMany()
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("zzqx", in: window)
        try await untilClose { window.answered != 0 }
        try XCTSkipIf(window.textTicket == 0, "the files were read before the names were seen")
        XCTAssertEqual(window.status, "Searching inside files…")
        try await until { window.textTicket == 0 }
        XCTAssertEqual(window.status, "No Results")
    }

    func testTheMadeUpNamesStillReadTheChosenFolders() async throws {
        try write("the zzqx plan", at: "notes/plan.txt")
        let window = try await opened(access: true, standIn: 1000, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("zzqx", in: window)
        try await until { window.answered != 0 && window.hits.count == 1 && window.textTicket == 0 }
        XCTAssertTrue(window.rows.isEmpty)
        XCTAssertNil(window.status, "never stuck on Searching inside files…")
    }

    func testFoldersAddedInTheHealthViewAreSearchedFromTheWindow() async throws {
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true)
        window.showHealth(nil)
        let health = try XCTUnwrap(window.health)
        stage.chosen = [root]
        health.addFolders()
        health.hide()
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.hits.count == 1 }
    }

    func testNothingFoundAnywhereSaysNoResultsOnceTheFilesAreRead() async throws {
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("zzqx", in: window)
        try await until { window.answered != 0 && window.textTicket == 0 }
        XCTAssertEqual(window.status, "No Results")
    }

    func testInsideAFolderOnlyTheChosenPartOfItIsRead() async throws {
        try write("the budget", at: "notes/plan.txt")
        try write("the budget", at: "other/plan.txt")
        let notes = realRoot.appendingPathComponent("notes").path
        // Chosen through /var, while Finder and the list name /private/var.
        XCTAssertNotEqual(root.path, realRoot.path)
        let window = try await opened(access: true, textFolders: [root.path], finder: { .folder(notes) })
        window.toggleInFinderFolder()
        try await until { window.shown }
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.answered != 0 && window.textTicket == 0 && !window.hits.isEmpty }
        XCTAssertEqual(window.hits.map(\.path), [notes + "/plan.txt"])
    }

    func testANewListKeepsTheLinesAndThePickedLine() async throws {
        try write("x", at: "notes/budget a.txt")
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.rows.count == 1 && window.hits.count == 1 && window.textTicket == 0 }
        down(window)
        XCTAssertEqual(window.table.selectedRow, 2)

        try write("budget again", at: "notes/budget b.txt")
        try await until { window.rows.count == 2 }
        XCTAssertFalse(window.hits.isEmpty, "the same search keeps its lines while it reads them again")
        try await until { window.hits.count == 2 && window.textTicket == 0 }
        XCTAssertEqual(window.table.selectedRow, 3 + window.hits.firstIndex { $0.path.hasSuffix("/plan.txt") }!)
        XCTAssertEqual(window.selected?.name, "plan.txt")
        XCTAssertEqual(window.header, 2)
        try await settled(window)
        XCTAssertEqual(window.table.rect(ofRow: 1).height, Metric.row, "the title's old row is a file's now")
        XCTAssertEqual(window.table.rect(ofRow: 2).height, Metric.header)
    }

    func testArrowsOrAClickWhileThePickedLineIsAwaitedKeepTheUsersRow() async throws {
        try write("x", at: "notes/budget a.txt")
        try write("the budget", at: "notes/plan.txt")
        let window = try await opened(access: true, textFolders: [root.path])
        window.show()
        defer { window.hide() }
        window.toggleText(nil)
        try await type("budget", in: window)
        try await until { window.rows.count == 1 && window.hits.count == 1 && window.textTicket == 0 }
        let waiting = (hit: window.hits[0], row: 2)

        window.repick = waiting
        down(window)
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.moveUp(_:))))
        XCTAssertNil(window.repick, "Down then Up back onto the waiting row is still the user's pick")

        window.repick = waiting
        _ = window.tableView(window.table, selectionIndexesForProposedSelection: [])
        XCTAssertNotNil(window.repick, "a click below the rows picks nothing")
        _ = window.tableView(window.table, selectionIndexesForProposedSelection: [2])
        XCTAssertNil(window.repick, "a click on the waiting row keeps that row")
    }

    func testACellChangesBetweenANameALineAndTheTitle() {
        let cell = ResultCell()
        let icon = NSImage()
        let line = TextHit(path: "/notes/plan.txt", text: "q3 budget", match: NSRange(location: 3, length: 6), line: 9)
        cell.show(line, icon: icon)
        XCTAssertEqual(cell.folder.stringValue, "plan.txt, line 9 · /notes")
        cell.backgroundStyle = .emphasized
        let color = cell.name.attributedStringValue.attribute(.foregroundColor, at: 0, effectiveRange: nil) as? NSColor
        XCTAssertEqual(color, .alternateSelectedControlTextColor, "a picked line reads on the accent colour")
        cell.backgroundStyle = .normal
        cell.showHeader("Inside Files")
        XCTAssertTrue(cell.icon.isHidden)
        XCTAssertTrue(cell.folder.isHidden)
        XCTAssertNil(cell.hit)
        cell.show(Row(name: "budget.txt", folder: "/notes", isFolder: false), icon: icon)
        XCTAssertFalse(cell.isHeader)
        XCTAssertFalse(cell.icon.isHidden)
        XCTAssertFalse(cell.folder.isHidden)
        XCTAssertEqual(cell.name.frame.origin, NSPoint(x: 48, y: 24))
        XCTAssertEqual(cell.name.font, .systemFont(ofSize: NSFont.systemFontSize))
        XCTAssertEqual(cell.name.textColor, .labelColor)
        XCTAssertEqual(cell.name.stringValue, "budget.txt")
        XCTAssertEqual(cell.folder.stringValue, "/notes")
        cell.show(line, icon: icon)
        cell.show(Row(name: "q3 budget", folder: "/notes", isFolder: false), icon: icon)
        let plain = cell.name.attributedStringValue.attribute(.font, at: 4, effectiveRange: nil) as? NSFont
        XCTAssertFalse(plain?.fontDescriptor.symbolicTraits.contains(.bold) ?? true, "no bold left from the line")
    }

    func testAMatchPastTheRowsEndIsBroughtIntoView() {
        let cell = ResultCell()
        let text = String(repeating: "lorem ipsum ", count: 12) + "naïve budget" + String(repeating: " dolor", count: 6)
        XCTAssertLessThan(text.utf8.count, 200, "the core sends a line this short whole")
        let at = (text as NSString).range(of: "budget")
        cell.show(TextHit(path: "/notes/plan.txt", text: text, match: at, line: 1), icon: NSImage())
        let shown = cell.name.attributedStringValue
        XCTAssertTrue(shown.string.hasPrefix("…"))
        XCTAssertTrue(shown.string.hasSuffix(" dolor"), "the end is kept; the label cuts it if it must")
        var bold = NSRange()
        let font = shown.attribute(.font, at: (shown.string as NSString).range(of: "budget").location, effectiveRange: &bold)
        XCTAssertEqual((shown.string as NSString).substring(with: bold), "budget")
        XCTAssertTrue((font as? NSFont)?.fontDescriptor.symbolicTraits.contains(.bold) ?? false)
        let upToMatch = shown.attributedSubstring(from: NSRange(location: 0, length: NSMaxRange(bold)))
        XCTAssertLessThan(upToMatch.size().width, cell.name.frame.width, "the match shows")
        XCTAssertTrue(shown.string.dropFirst().hasPrefix("ipsum ") || shown.string.dropFirst().hasPrefix("lorem "), "cut between words")

        let short = TextHit(path: "/notes/plan.txt", text: "q3 budget", match: NSRange(location: 3, length: 6), line: 2)
        cell.show(short, icon: NSImage())
        XCTAssertEqual(cell.name.stringValue, "q3 budget", "a line that fits is shown whole")

        let wrong = TextHit(path: "/notes/plan.txt", text: "q3", match: NSRange(location: 3, length: 6), line: 3)
        cell.show(wrong, icon: NSImage())
        XCTAssertEqual(cell.name.stringValue, "q3", "a match past the line's end is ignored")
        let font2 = cell.name.attributedStringValue.attribute(.font, at: 0, effectiveRange: nil) as? NSFont
        XCTAssertFalse(font2?.fontDescriptor.symbolicTraits.contains(.bold) ?? true)
    }

    /// Enough files without the words the tests look for that reading them takes a while.
    private func writeMany() throws {
        let filler = String(repeating: "nothing to see in this line\n", count: 400)
        for at in 0..<1500 { try write(filler, at: "filler/\(at % 30)/f\(at).txt") }
    }

    /// Polls every millisecond for a moment that may pass between two 10 ms polls; skips the test
    /// when the moment passed unseen.
    private func untilClose(_ done: () -> Bool) async throws {
        for _ in 0..<5000 where !done() { try await Task.sleep(for: .milliseconds(1)) }
        try XCTSkipUnless(done(), "the moment passed before it was seen")
    }

    private func write(_ text: String, at path: String) throws {
        let file = root.appendingPathComponent(path)
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        try text.write(to: file, atomically: true, encoding: .utf8)
    }

    /// `root` under /private/var, as the list holds it; Foundation's own resolving drops /private.
    private var realRoot: URL {
        URL(fileURLWithPath: String(cString: realpath(root.path, nil)), isDirectory: true)
    }

    /// Waits out the 0.15 s resize: rows below the window's edge get no view until it grows.
    private func settled(_ window: SearchPanel, file: StaticString = #filePath, line: UInt = #line) async throws {
        let scroll = try XCTUnwrap(window.table.enclosingScrollView, file: file, line: line)
        try await until({ abs(window.panel.frame.height - scroll.frame.maxY) < 0.5 }, file: file, line: line)
        window.panel.displayIfNeeded()
    }

    private func cell(_ window: SearchPanel, _ row: Int) -> ResultCell? {
        window.table.view(atColumn: 0, row: row, makeIfNecessary: false) as? ResultCell
    }

    private func assertOnScreen(_ window: SearchPanel, file: StaticString = #filePath, line: UInt = #line) {
        let table = window.table
        XCTAssertEqual(table.numberOfRows, window.rows.count, file: file, line: line)
        let visible = table.rows(in: table.visibleRect)
        XCTAssertGreaterThan(visible.length, 0, file: file, line: line)
        var icons: [String: NSImage] = [:]
        for row in visible.location..<min(NSMaxRange(visible), window.rows.count) {
            let item = window.rows[row]
            guard let cell = cell(window, row) else {
                XCTFail("\(item.name) has no view", file: file, line: line)
                continue
            }
            XCTAssertEqual(cell.name.stringValue, item.name, file: file, line: line)
            XCTAssertEqual(cell.folder.stringValue, item.folder, file: file, line: line)
            let style: NSView.BackgroundStyle = row == table.selectedRow ? .emphasized : .normal
            XCTAssertEqual(cell.backgroundStyle, style, item.name, file: file, line: line)
            guard let icon = cell.icon.image else {
                XCTFail("\(item.name) has no icon", file: file, line: line)
                continue
            }
            let kind = item.isApp ? item.path : item.isFolder ? "/" : (item.name as NSString).pathExtension.lowercased()
            if let seen = icons[kind] {
                XCTAssertTrue(seen === icon, "\(item.name) has another \(kind) icon", file: file, line: line)
            } else {
                XCTAssertFalse(icons.values.contains { $0 === icon }, "\(item.name) shares an icon", file: file, line: line)
                icons[kind] = icon
            }
        }
        table.enumerateAvailableRowViews { _, row in
            XCTAssertLessThan(row, window.rows.count, "a row past the list", file: file, line: line)
        }
    }

    /// The image as a row's 32-point icon view draws it.
    private func pixels(of image: NSImage, scale: Int, in appearance: NSAppearance) -> [UInt8]? {
        let view = NSImageView(frame: NSRect(x: 0, y: 0, width: 32, height: 32))
        view.appearance = appearance
        view.image = image
        let side = 32 * scale
        guard
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: side, pixelsHigh: side, bitsPerSample: 8, samplesPerPixel: 4,
                hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: side * 4, bitsPerPixel: 32)
        else { return nil }
        rep.size = view.bounds.size
        view.cacheDisplay(in: view.bounds, to: rep)
        return rep.bitmapData.map { Array(UnsafeBufferPointer(start: $0, count: side * side * 4)) }
    }

    /// How many bytes come out non-zero when `image` is drawn `side` pixels square on this thread.
    nonisolated private static func inked(_ image: NSImage, side: Int) -> Int {
        guard
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: side, pixelsHigh: side, bitsPerSample: 8, samplesPerPixel: 4,
                hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: side * 4, bitsPerPixel: 32),
            let context = NSGraphicsContext(bitmapImageRep: rep), let data = rep.bitmapData
        else { return 0 }
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = context
        image.draw(in: NSRect(x: 0, y: 0, width: side, height: side))
        NSGraphicsContext.restoreGraphicsState()
        return UnsafeBufferPointer(start: data, count: side * side * 4).filter { $0 != 0 }.count
    }

    private func item(_ window: SearchPanel) -> URL? {
        window.previewPanel(QLPreviewPanel.shared(), previewItemAt: 0)?.previewItemURL
    }

    private func previewed() -> URL? {
        QLPreviewPanel.shared().currentPreviewItem?.previewItemURL
    }

    private func key(
        _ characters: String, type: NSEvent.EventType = .keyDown, flags: NSEvent.ModifierFlags = []
    ) -> NSEvent {
        NSEvent.keyEvent(
            with: type, location: .zero, modifierFlags: flags, timestamp: 0, windowNumber: 0, context: nil,
            characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: 0)!
    }

    /// For the few tests that need the real screen: they bring windows forward and take the keys,
    /// so they run only with ARGUS_FOCUS_TESTS=1.
    private func onScreenOnly() throws {
        try XCTSkipUnless(
            ProcessInfo.processInfo.environment["ARGUS_FOCUS_TESTS"] == "1",
            "takes the screen; run with ARGUS_FOCUS_TESTS=1")
        onScreen = true
        // Started as main.swift starts the app; otherwise none of the runner's ordinary windows,
        // such as the preview, can take the keys.
        if NSApp.activationPolicy() != .accessory {
            NSApp.setActivationPolicy(.accessory)
            NSApp.finishLaunching()
        }
    }

    /// Whether `done` comes true within 1 s, for checks that skip rather than fail.
    private func soon(_ done: () -> Bool) async -> Bool {
        for _ in 0..<100 where !done() { try? await Task.sleep(for: .milliseconds(10)) }
        return done()
    }

    /// Lets every block already queued on the main queue run first.
    private func nextTurn() async {
        await withCheckedContinuation { done in DispatchQueue.main.async { done.resume() } }
    }

    private func type(_ text: String, in window: SearchPanel) async throws {
        window.field.stringValue = text
        window.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification))
    }

    private func tab(_ window: SearchPanel) -> Bool {
        window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertTab(_:)))
    }

    /// Makes a Backspace, held down or not, the event the app is handling.
    private func currentKey(repeating: Bool) throws {
        let event = try XCTUnwrap(
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: 0, context: nil,
                characters: "\u{7F}", charactersIgnoringModifiers: "\u{7F}", isARepeat: repeating, keyCode: 51))
        NSApp.postEvent(event, atStart: true)
        _ = NSApp.nextEvent(matching: .keyDown, until: .distantPast, inMode: .default, dequeue: true)
        XCTAssertEqual(NSApp.currentEvent?.isARepeat, repeating)
    }

    /// Later tests must not see a keystroke at time 0 as the one that started their search.
    private func clearCurrentEvent() {
        guard
            let event = NSEvent.otherEvent(
                with: .applicationDefined, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: 0,
                context: nil, subtype: 0, data1: 0, data2: 0)
        else { return }
        NSApp.postEvent(event, atStart: true)
        _ = NSApp.nextEvent(matching: .applicationDefined, until: .distantPast, inMode: .default, dequeue: true)
    }

    private func backspace(_ window: SearchPanel) -> Bool {
        window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.deleteBackward(_:)))
    }

    private func down(_ window: SearchPanel) {
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.moveDown(_:))))
    }

    /// Waits up to 5 s: a change takes FSEvents' 0.2 s, up to 0.5 s between rebuilds, and the rebuild.
    private func until(_ done: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async throws {
        for _ in 0..<500 where !done() { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(done(), file: file, line: line)
    }

    /// Waits for the scan, so its event can't reach the next test's core.
    /// Never the real Finder, which would ask whether the test runner may control it.
    private func opened(
        access: Bool, on stage: (any Stage)? = nil, standIn: UInt64? = nil, saved: String? = nil, opens: String? = nil,
        apps: [String] = [], textFolders: [String] = [], everything: IndexEverything = IndexEverything(),
        finder: @escaping @Sendable () -> FinderFolder = { .none }
    ) async throws -> SearchPanel {
        self.access = access
        let hasAccess = { [unowned self] in
            checks += 1
            return self.access
        }
        let folders = TextFolders()
        folders.add(textFolders)
        let window = SearchPanel(
            root: root.path, standIn: standIn, saved: saved, opens: opens, apps: apps, textFolders: folders, everything: everything,
            hasAccess: hasAccess, stage: stage ?? self.stage, finder: finder)
        for _ in 0..<500 where !window.indexed { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(window.indexed)
        return window
    }

    private func assertFits(_ window: SearchPanel, file: StaticString = #filePath, line: UInt = #line) {
        let needed = window.barText.cell?.cellSize.width ?? .infinity
        XCTAssertLessThanOrEqual(needed, window.barText.frame.width, window.barText.stringValue, file: file, line: line)
    }
}

/// Counts calls to a stand-in Finder, which come from background threads.
/// Holds a stand-in Finder until the test lets it answer.
private final class Gate: @unchecked Sendable {
    private let lock = NSCondition()
    private var opened = false
    private var done = false

    var answered: Bool { lock.withLock { done } }

    func wait() {
        lock.withLock {
            while !opened { lock.wait() }
            done = true
        }
    }

    func open() {
        lock.withLock {
            opened = true
            lock.broadcast()
        }
    }

    func reset() {
        lock.withLock { (opened, done) = (false, false) }
    }
}

private final class Asked: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0

    func next() -> Int {
        lock.withLock {
            count += 1
            return count
        }
    }
}
