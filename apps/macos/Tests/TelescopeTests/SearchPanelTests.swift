import AppKit
import QuickLookUI
import XCTest

@testable import Telescope

@MainActor
final class SearchPanelTests: XCTestCase {
    private var root: URL!
    private var access = false
    private var checks = 0
    private var stage: OffStage!

    override func setUp() async throws {
        _ = NSApplication.shared
        stage = OffStage()
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

    func testTabOnAFileOrNoResultsIsLeftAlone() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: true)
        XCTAssertFalse(tab(window), "an empty box")
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        XCTAssertFalse(tab(window))
        try await type("zzzzqqq", in: window)
        try await until { window.rows.isEmpty && window.panel.frame.height > Metric.field }
        XCTAssertFalse(tab(window))
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
        XCTAssertFalse(tab(window), "a file")
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
        XCTAssertFalse(stage.isActive, "the window takes the keys without making Telescope the active app")
    }

    func testAPreviewClosedWhileTelescopeIsActiveHandsTheKeysBackToTheBox() async throws {
        try Data().write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false)
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        window.panel.makeFirstResponder(window.settings)

        // A click on the preview makes Telescope the active app and gives the preview the keys.
        stage.isActive = true
        stage.key = nil
        stage.previewIsKey = true
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: window.panel))
        await nextTurn()
        XCTAssertTrue(window.shown, "a click on the preview keeps the window")
        XCTAssertNil(stage.key, "the preview keeps the keys")

        // Esc on the preview: it lets go of the keys and nothing else picks them up.
        window.closePreview()
        stage.previewIsKey = false
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: QLPreviewPanel.shared()))
        await nextTurn()
        XCTAssertTrue(window.shown)
        XCTAssertTrue(stage.isKey(window.panel))
        XCTAssertTrue(window.panel.firstResponder === window.field.currentEditor())
    }

    func testOnScreenAPreviewClosedWhileTelescopeIsActiveHandsTheKeysBackToTheBox() async throws {
        try onScreenOnly()
        try Data("budget\n".utf8).write(to: root.appendingPathComponent("budget.txt"))
        let window = try await opened(access: false, on: Screen())
        window.show()
        defer { window.hide() }
        try await type("budget", in: window)
        try await until { window.rows.count == 1 }
        window.quickLook(nil)
        try await until { window.previewing }
        let preview = QLPreviewPanel.shared()!
        try await until { preview.currentController as AnyObject === window.panel }

        // A click on the preview makes Telescope the active app and gives the preview the keys.
        NSApp.activate(ignoringOtherApps: true)
        let unlocked = await soon { NSApp.isActive }
        try XCTSkipUnless(unlocked, "needs an unlocked screen")
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
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification, object: QLPreviewPanel.shared()))
        await nextTurn()
        XCTAssertTrue(window.shown)
        XCTAssertTrue(window.panel.firstResponder === window.settings)
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

        // A click in another app: no window of Telescope's has the keys, and it isn't active.
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
        stage.key = nil
        stage.isActive = true
        window.panel.makeFirstResponder(window.settings)
        window.settleKeys()
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
        try await until { !stage.previewShows }
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
        try await until { !stage.previewShows }
        await nextTurn()
        XCTAssertFalse(stage.previewShows, "stays closed")
        XCTAssertEqual(stage.closes, 1)
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

    /// For the few tests that need the real screen: they bring windows forward and take the keys,
    /// so they run only with TELESCOPE_FOCUS_TESTS=1.
    private func onScreenOnly() throws {
        try XCTSkipUnless(
            ProcessInfo.processInfo.environment["TELESCOPE_FOCUS_TESTS"] == "1",
            "takes the screen; run with TELESCOPE_FOCUS_TESTS=1")
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
    private func opened(access: Bool, on stage: (any Stage)? = nil) async throws -> SearchPanel {
        self.access = access
        let hasAccess = { [unowned self] in
            checks += 1
            return self.access
        }
        let window = SearchPanel(root: root.path, hasAccess: hasAccess, stage: stage ?? self.stage)
        for _ in 0..<500 where !window.indexed { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(window.indexed)
        return window
    }

    private func assertFits(_ window: SearchPanel, file: StaticString = #filePath, line: UInt = #line) {
        let needed = window.barText.cell?.cellSize.width ?? .infinity
        XCTAssertLessThanOrEqual(needed, window.barText.frame.width, window.barText.stringValue, file: file, line: line)
    }
}
