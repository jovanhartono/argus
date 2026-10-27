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
        let window = try await opened(access: true)
        let preview = QLPreviewPanel.shared()!
        XCTAssertEqual(window.numberOfPreviewItems(in: preview), 0)
        XCTAssertNil(window.previewPanel(preview, previewItemAt: 0))
        window.quickLook(nil)
        XCTAssertFalse(window.previewing)
        XCTAssertNil(window.missing)
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
        try await Task.sleep(for: .milliseconds(100))
        try XCTSkipUnless(NSApp.isActive && window.panel.isKeyWindow, "needs an unlocked screen")
        let preview = QLPreviewPanel.shared()!
        window.panel.beginPreviewPanelControl(preview)
        window.panel.makeFirstResponder(window.settings)
        preview.makeKeyAndOrderFront(nil)
        try await until { preview.isKeyWindow }
        try await Task.sleep(for: .milliseconds(50))
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
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        XCTAssertFalse(NSApp.isActive)

        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: QLPreviewPanel.shared()))
        XCTAssertTrue(window.shown, "decided once macOS has picked the next key window")
        try await until { !window.shown }
        try await until { !window.previewing }
    }

    func testFocusLeavingTheWindowWithoutAPreviewHidesItAtOnce() async throws {
        let window = try await opened(access: true)
        window.show()
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: window.panel))
        XCTAssertFalse(window.shown)
    }

    func testAHiddenWindowIgnoresTheKeysSettling() async throws {
        let window = try await opened(access: true)
        window.settleKeys()
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(window.shown)
        XCTAssertFalse(window.panel.isVisible)
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
        try XCTSkipUnless(window.panel.isKeyWindow, "needs an unlocked screen")
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

    func testAFileGoneWhileShownClosesThePreviewAndSaysSo() async throws {
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

        // Gone before the list catches up, so the next row is still listed.
        try FileManager.default.removeItem(atPath: window.rows[1].path)
        down(window)
        XCTAssertEqual(window.missing, "“\(window.rows[1].name)” can’t be found.")
        try await until { !window.previewing }
    }

    private func item(_ window: SearchPanel) -> URL? {
        window.previewPanel(QLPreviewPanel.shared(), previewItemAt: 0)?.previewItemURL
    }

    private func previewed() -> URL? {
        QLPreviewPanel.shared().currentPreviewItem?.previewItemURL
    }

    private func key(_ characters: String) -> NSEvent {
        NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: 0, context: nil,
            characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: 0)!
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
