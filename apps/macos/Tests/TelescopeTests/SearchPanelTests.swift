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
    }

    func testANewListRewritesTheRowsOnScreenInPlace() async throws {
        let notes = root.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: false)
        for name in ["budget a.txt", "budget b.txt", "report.txt"] {
            try Data().write(to: notes.appendingPathComponent(name))
        }
        try FileManager.default.createDirectory(
            at: notes.appendingPathComponent("budget c"), withIntermediateDirectories: false)
        let window = try await opened(access: true)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        try await settled(window)
        assertOnScreen(window)
        let top = try XCTUnwrap(cell(window, 0))

        try await type("report", in: window)
        try await until { window.rows.map(\.name) == ["report.txt"] }
        try await settled(window)
        XCTAssertEqual(window.table.numberOfRows, 1)
        XCTAssertTrue(cell(window, 0) === top, "the row is kept and only its words change")
        assertOnScreen(window)

        try await type("budget", in: window)
        try await until { window.rows.count == 3 }
        try await settled(window)
        assertOnScreen(window)
        let icons = window.rows.indices.map { cell(window, $0)?.icon.image }
        let files = window.rows.indices.filter { !window.rows[$0].isFolder }
        let folder = try XCTUnwrap(window.rows.firstIndex(where: \.isFolder))
        XCTAssertTrue(icons[files[0]] === icons[files[1]], "one icon per kind of file")
        XCTAssertFalse(icons[files[0]] === icons[folder])

        try await type("", in: window)
        XCTAssertEqual(window.table.numberOfRows, 0)
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
        assertOnScreen(window)

        // The same number of rows, so none is added or dropped: each one on screen is rewritten.
        try await type("report", in: window)
        try await until { window.rows.count == 12 && window.rows.allSatisfy { $0.name.hasPrefix("report") } }
        try await settled(window)
        assertOnScreen(window)
        window.table.scrollRowToVisible(11)
        window.panel.displayIfNeeded()
        assertOnScreen(window)
    }

    func testRowIconsLookLikeFindersOwn() async throws {
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
        let finder = NSWorkspace.shared.icon(for: .pdf)
        let pixels = try XCTUnwrap(self.pixels(of: drawn))
        XCTAssertGreaterThan(pixels.filter { $0 != 0 }.count, pixels.count / 4, "not blank")
        XCTAssertEqual(pixels, self.pixels(of: finder))
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

    /// Waits out the 0.15 s resize: rows below the window's edge get no view until it grows.
    private func settled(_ window: SearchPanel) async throws {
        let rows = CGFloat(min(window.rows.count, Metric.visibleRows))
        try await until { window.panel.frame.height == Metric.field + 1 + rows * Metric.row + 2 * Metric.inset }
        window.panel.displayIfNeeded()
    }

    private func cell(_ window: SearchPanel, _ row: Int) -> ResultCell? {
        window.table.view(atColumn: 0, row: row, makeIfNecessary: false) as? ResultCell
    }

    /// Every row with a view on screen shows its own result, and at least one does.
    private func assertOnScreen(_ window: SearchPanel, file: StaticString = #filePath, line: UInt = #line) {
        var seen = 0
        window.table.enumerateAvailableRowViews { view, row in
            guard let cell = view.view(atColumn: 0) as? ResultCell else { return }
            seen += 1
            XCTAssertLessThan(row, window.rows.count, file: file, line: line)
            guard row < window.rows.count else { return }
            XCTAssertEqual(cell.name.stringValue, window.rows[row].name, file: file, line: line)
            XCTAssertEqual(cell.folder.stringValue, window.rows[row].folder, file: file, line: line)
        }
        XCTAssertGreaterThan(seen, 0, file: file, line: line)
    }

    /// The image as a row's 32-point icon view draws it, on a Retina screen.
    private func pixels(of image: NSImage) -> [UInt8]? {
        let view = NSImageView(frame: NSRect(x: 0, y: 0, width: 32, height: 32))
        view.image = image
        guard
            let rep = NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: 64, pixelsHigh: 64, bitsPerSample: 8, samplesPerPixel: 4,
                hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 64 * 4, bitsPerPixel: 32)
        else { return nil }
        rep.size = view.bounds.size
        view.cacheDisplay(in: view.bounds, to: rep)
        return rep.bitmapData.map { Array(UnsafeBufferPointer(start: $0, count: 64 * 64 * 4)) }
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
