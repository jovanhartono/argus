import AppKit
import XCTest

@testable import Argus

/// Draws the frames of the site's demo video from a real search window, off screen, searching
/// made-up files. scripts/make-site-video.sh runs it with ARGUS_SITE_FRAMES set to a folder.
@MainActor
final class SiteFramesTests: XCTestCase {
    private var root: URL!
    private var out: URL!
    private var stage: OffStage!
    /// Each frame's file and how long it holds, in seconds.
    private var shots: [(String, Double)] = []
    private var canvas = NSSize(width: 864, height: 540)
    /// Where the window's left edge sits; nil centres it.
    private var left: CGFloat?

    private static let top: CGFloat = 40
    /// Three frames at 30 fps.
    private static let letter = 0.1

    override func setUp() async throws {
        _ = NSApplication.shared
        guard let folder = ProcessInfo.processInfo.environment["ARGUS_SITE_FRAMES"] else {
            throw XCTSkip("draws the site's video; run scripts/make-site-video.sh")
        }
        UserDefaults.keepHotkeysInMemory()
        stage = OffStage()
        out = URL(fileURLWithPath: folder, isDirectory: true)
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        root = try SitePage.makeHome()
    }

    override func tearDown() async throws {
        SitePage.leaveHome()
        XCTAssertEqual(NSApp.windows.filter(\.isVisible), [], "a test put a window on the screen")
        XCTAssertFalse(NSApp.isActive, "a test made the test runner the active app")
    }

    func testFramesForTheSite() async throws {
        try makeFiles()
        XCTAssertEqual(ResultCell.place(root.path + "/Documents"), "~/Documents", "no real path in a frame")
        let window = try await opened()
        window.show()
        defer { window.hide() }
        try await until { window.panel.frame.height == Metric.field }

        try shoot(window, "empty", 0.7)
        for count in 1...6 {
            try await type(String("desing".prefix(count)), in: window)
            try shoot(window, "desing-\(count)", count == 6 ? 2.2 : Self.letter)
        }
        XCTAssertEqual(window.selected?.name, "design.key")

        XCTAssertTrue(window.control(NSTextField(), textView: NSTextView(), doCommandBy: #selector(NSResponder.cancelOperation(_:))))
        try await until { window.panel.frame.height == Metric.field }
        try shoot(window, "cleared", 0.35)
        window.toggleText(nil)
        try shoot(window, "inside-files", 0.6)
        // All at once: letter by letter, the files come back in a different order each time and
        // the selection jumps between them.
        try await type("kickoff", in: window)
        try await until { window.textState == .done }
        try await settled(window)
        try shoot(window, "kickoff", 2.6)
        XCTAssertGreaterThan(window.hits.count, 0)
        try writeList()
    }

    /// The second video, in its own folder: a project folder looked through as in Finder, then
    /// searched inside.
    func testBrowseFramesForTheSite() async throws {
        try makeFiles()
        try fillAtlas()
        out = out.appendingPathComponent("browse", isDirectory: true)
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        // Wide enough for the details pane; the box stays put as the pane grows to its right.
        canvas = NSSize(width: 1100, height: 648)
        left = ((canvas.width - Metric.width - 1 - Metric.pane) / 2).rounded()
        XCTAssertEqual(DetailsFormat().folder(root.path + "/Documents"), "~/Documents", "no real path in a frame")
        let window = try await opened()
        window.show()
        defer { window.hide() }
        try await until { window.panel.frame.height == Metric.field }

        try shoot(window, "empty", 0.5)
        for count in 1...5 {
            try await type(String("atlas".prefix(count)), in: window)
            try shoot(window, "atlas-\(count)", count == 5 ? 1.0 : Self.letter)
        }
        XCTAssertEqual(window.selected?.name, "Atlas")

        window.toggleDetails(nil)
        try await until { window.pane.children.count == 6 && window.pane.waiting == nil }
        try await settled(window)
        try shoot(window, "details", 1.6)

        XCTAssertTrue(key(#selector(NSResponder.moveRight(_:)), in: window))
        try await until { window.picked?.name == "Designs" }
        try await settled(window)
        try shoot(window, "inside-atlas", 0.9)
        XCTAssertTrue(key(#selector(NSResponder.moveRight(_:)), in: window))
        try await until { window.picked?.name == "home-screen.png" }
        try await settled(window)
        try shoot(window, "inside-designs", 1.0)
        XCTAssertTrue(key(#selector(NSResponder.moveDown(_:)), in: window))
        try shoot(window, "onboarding", 0.9)
        XCTAssertTrue(key(#selector(NSResponder.moveLeft(_:)), in: window))
        XCTAssertEqual(window.picked?.name, "Designs")
        try shoot(window, "back-in-atlas", 0.7)
        // Tab searches the list's folder, not the pane's pick, so Atlas is shown picked first.
        XCTAssertTrue(key(#selector(NSResponder.cancelOperation(_:)), in: window))
        try await settled(window)
        try shoot(window, "atlas-again", 0.6)

        XCTAssertTrue(key(#selector(NSResponder.insertTab(_:)), in: window))
        try await until { window.scopes.map(\.name) == ["Atlas"] }
        try await until { window.panel.frame.size == NSSize(width: Metric.width, height: Metric.field) }
        try shoot(window, "search-in-atlas", 0.8)
        for count in 1...4 {
            try await type(String("road".prefix(count)), in: window)
            if count == 4 { try await until { window.pane.waiting == nil } }
            try shoot(window, "road-\(count)", count == 4 ? 2.2 : Self.letter)
        }
        XCTAssertEqual(window.selected?.name, "roadmap.txt")
        XCTAssertNotNil(window.pane.thumbnailed, "the pane shows the file itself")
        try writeList()
    }

    private func opened() async throws -> SearchPanel {
        let window = SearchPanel(root: root.path, hasAccess: { true }, stage: stage, finder: { .none })
        window.panel.appearance = NSAppearance(named: .aqua)
        try await until { window.indexed }
        return window
    }

    /// A key as the search box hands it on, with the text cursor at the end.
    private func key(_ selector: Selector, in window: SearchPanel) -> Bool {
        let editor = NSTextView()
        editor.string = window.field.stringValue
        editor.setSelectedRange(NSRange(location: (editor.string as NSString).length, length: 0))
        return window.control(NSTextField(), textView: editor, doCommandBy: selector)
    }

    /// The Atlas project as someone's real one looks: sizes, and changes spread over a week, so
    /// the pane's newest-first list reads Designs, roadmap, brief, deck, moodboard, logo.
    private func fillAtlas() throws {
        let atlas = "Documents/Work/Atlas/"
        try write("", at: "Documents/Work/atlas-budget.numbers")
        try write("", at: "Desktop/atlas-standup.pages")
        try write(
            """
            Atlas roadmap

            Q1  kickoff and research
                - interviews with ten customers
                - pick the stack, set up CI
            Q2  first beta
                - booking flow, reminders
                - invite the first five teams
            Q3  public launch
                - pricing page, onboarding video
                - support inbox and docs
            Q4  mobile app
            """, at: atlas + "roadmap.txt")
        let files: [(String, Int64, Double)] = [
            ("Designs/home-screen.png", 1_400_000, 0.1), ("Designs/onboarding.mov", 18_600_000, 0.8),
            ("Designs/app-icon.png", 212_000, 26), ("Designs/palette.png", 64_000, 75),
            ("roadmap.txt", 0, 0.7), ("brief.docx", 184_000, 3), ("pitch.key", 4_200_000, 27),
            ("moodboard.jpg", 2_100_000, 50), ("logo.png", 38_000, 140),
        ]
        let now = Date()
        for (name, size, hours) in files {
            let url = root.appendingPathComponent(atlas + name)
            if !FileManager.default.fileExists(atPath: url.path) { try write("", at: atlas + name) }
            if size > 0 {
                let handle = try FileHandle(forWritingTo: url)
                try handle.truncate(atOffset: UInt64(size))
                try handle.close()
            }
            try touch(atlas + name, now.addingTimeInterval(-hours * 3600))
        }
        try touch(atlas + "Designs", now.addingTimeInterval(-0.1 * 3600))
        try touch(atlas, now.addingTimeInterval(-0.1 * 3600))
    }

    private func touch(_ path: String, _ modified: Date) throws {
        try FileManager.default.setAttributes(
            [.modificationDate: modified, .creationDate: modified.addingTimeInterval(-21 * 24 * 3600)],
            ofItemAtPath: root.appendingPathComponent(path).path)
    }

    /// A made-up home folder: a few projects, notes and the usual folders.
    private func makeFiles() throws {
        let files = [
            // Icons come from the apps on the Mac that draws the video: a .pdf showed Chrome's logo.
            // With Office or Figma installed, .docx, .xlsx and .fig would too; look before publishing.
            "Documents/design.key", "Documents/Work/design-review.key", "Documents/Work/deadlines.numbers",
            "Documents/Work/data-export.csv", "Documents/Work/Atlas/brief.docx", "Documents/Work/Atlas/logo.png",
            "Documents/Personal/dentist-invoice.numbers", "Documents/Personal/travel-plan.pages",
            "Desktop/desk-plan.xlsx", "Desktop/screenshot-ideas.png", "Downloads/design-system.fig",
            "Downloads/dmg-installer-notes.txt", "Music/drum-loops/loop-01.wav", "Pictures/Dogs/beach.jpg",
        ]
        for file in files { try write("", at: file) }
        try FileManager.default.createDirectory(
            at: root.appendingPathComponent("Documents/Work/Atlas/Designs"), withIntermediateDirectories: true)
        try write(
            """
            # Atlas design notes

            Kickoff with the Atlas team on Monday at 10:00.
            Keep the accent green, a little darker in dark mode.
            """, at: "Documents/Notes/design-notes.md")
        try write(
            """
            Standup, Tuesday
            - Ana: landing page copy, then the kickoff deck
            - Sam: fixing the export crash
            """, at: "Documents/Notes/daily-standup.md")
        try write(
            """
            Atlas roadmap
            Q1  kickoff and research
            Q2  first beta
            """, at: "Documents/Work/Atlas/roadmap.txt")
    }

    private func write(_ text: String, at path: String) throws {
        let file = root.appendingPathComponent(path)
        try FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        try text.write(to: file, atomically: true, encoding: .utf8)
    }

    /// Waits for the answer to what was just typed, then for the window to finish growing.
    private func type(_ text: String, in window: SearchPanel) async throws {
        let before = window.answered
        window.field.stringValue = text
        window.field.currentEditor()?.selectedRange = NSRange(location: (text as NSString).length, length: 0)
        window.controlTextDidChange(Notification(name: NSControl.textDidChangeNotification))
        try await until { window.answered > before }
        try await settled(window)
    }

    private func settled(_ window: SearchPanel) async throws {
        let scroll = try XCTUnwrap(window.table.enclosingScrollView)
        let width = Metric.width + (window.detailing ? 1 + Metric.pane : 0)
        try await until {
            window.panel.frame.width == width
                && (scroll.isHidden || abs(window.panel.frame.height - scroll.frame.maxY) < 0.5)
        }
        window.panel.displayIfNeeded()
    }

    private func until(_ done: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async throws {
        for _ in 0..<500 where !done() { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(done(), file: file, line: line)
    }

    /// The window on the site's page, with a caret after the text.
    private func shoot(_ window: SearchPanel, _ name: String, _ seconds: Double) throws {
        let content = try XCTUnwrap(window.panel.contentView)
        content.layoutSubtreeIfNeeded()
        let size = content.bounds.size
        let views = try SitePage.bitmap(size)
        content.cacheDisplay(in: content.bounds, to: views)

        let frame = NSRect(
            x: left ?? ((canvas.width - size.width) / 2).rounded(), y: canvas.height - Self.top - size.height,
            width: size.width, height: size.height)
        let page = try SitePage.window(views, at: frame, on: canvas, corner: 16) {
            let typed = NSAttributedString(
                string: window.field.stringValue, attributes: [.font: window.field.font as Any]
            ).size().width
            let field = window.field.frame
            NSColor.controlAccentColor.setFill()
            NSRect(x: frame.minX + field.minX + 2 + typed, y: frame.maxY - field.maxY + 3, width: 2, height: field.height - 6)
                .fill()
        }

        let file = String(format: "%03d-%@.png", shots.count, name)
        try XCTUnwrap(page.representation(using: .png, properties: [:])).write(to: out.appendingPathComponent(file))
        shots.append((file, seconds))
    }

    /// ffmpeg's concat list. ffmpeg drops the last file's hold unless it is named again, and then
    /// holds it twice; the script cuts the video at the sum of the holds. At its own 25 fps an
    /// image would hold 0.08 s, then 0.12 s, for letters typed 0.1 s apart.
    private func writeList() throws {
        let entry = { (file: String) in "file '\(file)'\noption framerate 30\n" }
        var list = shots.map { entry($0.0) + "duration \($0.1)\n" }.joined()
        if let last = shots.last { list += entry(last.0) }
        try list.write(to: out.appendingPathComponent("frames.txt"), atomically: true, encoding: .utf8)
    }
}
