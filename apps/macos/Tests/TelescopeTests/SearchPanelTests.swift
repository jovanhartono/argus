import AppKit
import QuickLookUI
import XCTest

@testable import Telescope

@MainActor
final class SearchPanelTests: XCTestCase {
    private var root: URL!
    private var access = false
    private var checks = 0

    override func setUp() async throws {
        _ = NSApplication.shared
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: root)
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

    func testTheMadeUpNamesNeverShowTheLine() async throws {
        setenv("TELESCOPE_STAND_IN", "0", 1)
        defer { unsetenv("TELESCOPE_STAND_IN") }
        let window = try await opened(access: false)
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

    func testTabIsLeftAloneWithoutAButton() async throws {
        let granted = try await opened(access: false)
        access = true
        granted.recheckAccess()
        XCTAssertFalse(granted.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertTab(_:))))

        let full = try await opened(access: true)
        XCTAssertFalse(full.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.insertTab(_:))))
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

    func testAPreviewClosedWhileTelescopeIsActiveHandsTheKeysBackToTheBox() async throws {
        let window = try await opened(access: false)
        window.show()
        defer { window.hide() }
        NSApp.activate(ignoringOtherApps: true)
        let unlocked = await soon { NSApp.isActive && window.panel.isKeyWindow }
        try XCTSkipUnless(unlocked, "needs an unlocked screen")
        let preview = QLPreviewPanel.shared()!
        window.panel.beginPreviewPanelControl(preview)
        window.panel.makeFirstResponder(window.settings)
        preview.makeKeyAndOrderFront(nil)
        try await until { preview.isKeyWindow }
        await nextTurn()
        XCTAssertTrue(window.shown, "a click on the preview keeps the window")

        preview.orderOut(nil)
        try await until { window.panel.isKeyWindow }
        try await until { window.panel.firstResponder === window.field.currentEditor() }
        XCTAssertTrue(window.shown)
    }

    func testFocusLeavingTelescopeWithAPreviewUpHidesBoth() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }

        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: QLPreviewPanel.shared()))
        XCTAssertTrue(window.shown, "decided once macOS has picked the next key window")
        // Another test may have left Telescope active; on a locked screen it never is.
        NSApp.deactivate()
        try await until { !window.shown }
        try await until { !window.previewing }
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
        window.panel.makeFirstResponder(window.settings)
        window.settleKeys()
        await nextTurn()
        XCTAssertFalse(window.shown)
        XCTAssertTrue(window.panel.firstResponder === window.settings, "the box isn't handed the keys")
        try await until { !window.panel.isVisible }
        XCTAssertFalse(window.panel.isKeyWindow)
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
        XCTAssertFalse(QLPreviewPanel.shared().isVisible)
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
        try await until { !QLPreviewPanel.shared().isVisible }
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
        XCTAssertTrue(QLPreviewPanel.shared().isVisible)
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: esc))
        XCTAssertEqual(window.field.stringValue, "")
        try await until { !QLPreviewPanel.shared().isVisible }
        await nextTurn()
        XCTAssertFalse(QLPreviewPanel.shared().isVisible, "stays closed")
        XCTAssertTrue(window.shown)
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
        XCTAssertTrue(window.shown)

        down(window)
        XCTAssertNil(window.missing, "the first arrow brings the list back")
        XCTAssertEqual(window.selected, row, "on the same row")
    }

    func testAMadeUpNameSaysItCantBeFoundInsteadOfPreviewing() async throws {
        setenv("TELESCOPE_STAND_IN", "1000", 1)
        defer { unsetenv("TELESCOPE_STAND_IN") }
        let window = try await opened(access: true)
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
        XCTAssertEqual(window.field.stringValue, "budget")
        XCTAssertTrue(window.shown)

        window.quickLook(nil)
        try await until { window.previewing }
        window.quickLook(nil)
        try await until { !window.previewing }
        XCTAssertTrue(window.shown, "⌘Y again closes only the preview")
    }

    func testQuickLookFindsTheWindowAndShowsTheSelectedFile() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt"] {
            try Data("budget\n".utf8).write(to: notes.appendingPathComponent(name))
        }
        let window = try await opened(access: true)
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

        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        window.hide()
        try await until { !window.previewing }
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
        XCTAssertTrue(window.pane.summary.stringValue.hasSuffix("12 bytes"), window.pane.summary.stringValue)
        XCTAssertEqual(window.pane.place.stringValue, window.rows[0].folder)
        XCTAssertNotEqual(window.pane.modified.stringValue, "—")

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
        setenv("TELESCOPE_STAND_IN", "1000", 1)
        defer { unsetenv("TELESCOPE_STAND_IN") }
        let window = try await opened(access: true)
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
            let kind = item.isFolder ? "/" : (item.name as NSString).pathExtension.lowercased()
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

    private func down(_ window: SearchPanel) {
        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.moveDown(_:))))
    }

    /// Waits up to 5 s: a change takes FSEvents' 0.2 s, up to 0.5 s between rebuilds, and the rebuild.
    private func until(_ done: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async throws {
        for _ in 0..<500 where !done() { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(done(), file: file, line: line)
    }

    /// Waits for the scan, so its event can't reach the next test's core.
    private func opened(access: Bool) async throws -> SearchPanel {
        self.access = access
        let window = SearchPanel(root: root.path) { [unowned self] in
            checks += 1
            return self.access
        }
        for _ in 0..<500 where !window.indexed { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(window.indexed)
        return window
    }

    private func assertFits(_ window: SearchPanel, file: StaticString = #filePath, line: UInt = #line) {
        let needed = window.barText.cell?.cellSize.width ?? .infinity
        XCTAssertLessThanOrEqual(needed, window.barText.frame.width, window.barText.stringValue, file: file, line: line)
    }
}
