import AppKit
import XCTest

@testable import Telescope

final class HealthFormatTests: XCTestCase {
    private let english = HealthFormat(locale: Locale(identifier: "en_US"))
    private let german = HealthFormat(locale: Locale(identifier: "de_DE"))
    private let now = Date(timeIntervalSince1970: 1_800_000_000)

    func testEntriesAreGroupedInYourLanguage() {
        XCTAssertEqual(english.entries(Health(entries: 48_127)), "48,127")
        XCTAssertEqual(german.entries(Health(entries: 48_127)), "48.127")
        XCTAssertEqual(english.entries(Health(entries: 2_412_331)), "2,412,331")
        XCTAssertEqual(english.entries(Health()), "0")
    }

    func testDurationsUnderASecondMinuteAndAbove() {
        XCTAssertEqual(english.duration(ms: 0), "0 ms")
        XCTAssertEqual(english.duration(ms: 999), "999 ms")
        XCTAssertEqual(english.duration(ms: 1_000), "1.0 s")
        XCTAssertEqual(english.duration(ms: 1_549), "1.5 s")
        XCTAssertEqual(german.duration(ms: 1_549), "1,5 s")
        XCTAssertEqual(english.duration(ms: 59_900), "59.9 s")
        XCTAssertEqual(english.duration(ms: 60_000), "1 min 0 s")
        XCTAssertEqual(english.duration(ms: 125_400), "2 min 5 s")
        XCTAssertEqual(english.duration(ms: .max), "\(UInt64.max / 60_000) min \(UInt64.max / 1_000 % 60) s")
    }

    func testAgesReadAsFinderWouldSayThem() {
        let ago = { (seconds: TimeInterval) in self.english.ago(self.now.addingTimeInterval(-seconds), now: self.now) }
        XCTAssertEqual(ago(0), "Just now")
        XCTAssertEqual(ago(59), "Just now")
        XCTAssertEqual(ago(60), "1 minute ago")
        XCTAssertEqual(ago(5 * 60), "5 minutes ago")
        XCTAssertEqual(ago(2 * 3_600), "2 hours ago")
        XCTAssertEqual(ago(86_400), "Yesterday")
        XCTAssertEqual(ago(3 * 86_400), "3 days ago")
        XCTAssertEqual(ago(-3_600), "Just now", "a clock set back")
    }

    func testTheLastScanSaysWhenAndHowLongOrWhyThereWasNone() {
        let scanned = Health(entries: 48_127, scannedAt: now.addingTimeInterval(-300), scanMs: 1_549)
        XCTAssertEqual(english.scan(scanned, now: now), "5 minutes ago, took 1.5 s")
        let fresh = Health(entries: 3, scannedAt: now, scanMs: 120)
        XCTAssertEqual(english.scan(fresh, now: now), "Just now, took 120 ms")
        XCTAssertEqual(english.scan(Health(busy: true), now: now), "Under way")
        let opened = Health(entries: 48_127)
        XCTAssertEqual(english.scan(opened, now: now), "Not since launch; the saved list opened instead")
        // Still the saved list while its rebuild runs.
        XCTAssertEqual(english.scan(Health(entries: 48_127, busy: true), now: now), english.scan(opened, now: now))
        XCTAssertEqual(english.scan(Health(stopped: true), now: now), "Didn’t finish", "the first scan failed")
        XCTAssertEqual(english.scan(Health(entries: 48_127, stopped: true), now: now), english.scan(opened, now: now))
        XCTAssertEqual(english.scan(Health(scannedAt: now, scanMs: 5, stopped: true), now: now), "Just now, took 5 ms")
    }

    func testTheLastChangeTheStateAndAccess() {
        XCTAssertEqual(english.change(Health(), now: now), "None since launch")
        XCTAssertEqual(english.change(Health(changedAt: now.addingTimeInterval(-120)), now: now), "2 minutes ago")
        XCTAssertEqual(english.state(Health()), "Up to date")
        XCTAssertEqual(english.state(Health(busy: true)), "Updating…")
        XCTAssertEqual(english.state(Health(behind: true)), "Missing a change: rebuild the index")
        XCTAssertEqual(english.state(Health(busy: true, behind: true)), "Updating…", "the rebuild reads it")
        XCTAssertEqual(english.state(Health(stopped: true)), "Stopped: quit and reopen Telescope")
        XCTAssertEqual(english.state(Health(busy: true, behind: true, stopped: true)), english.state(Health(stopped: true)))
        XCTAssertEqual(english.access(true), "On")
        XCTAssertEqual(english.access(false), "Off: Desktop, Documents, Downloads and cloud drives are left out")
    }
}

@MainActor
final class HealthWindowTests: XCTestCase {
    private var stage: OffStage!
    private var reads = 0
    private var rebuilds = 0
    private var fullDisk = true
    private var health = Health(entries: 48_127, scannedAt: Date(), scanMs: 90)
    private var folders: TextFolders!

    override func setUp() async throws {
        _ = NSApplication.shared
        stage = OffStage()
        folders = TextFolders()
    }

    override func tearDown() async throws {
        XCTAssertEqual(NSApp.windows.filter(\.isVisible), [], "a test put a window on the screen")
    }

    private func until(_ done: () -> Bool, file: StaticString = #filePath, line: UInt = #line) async throws {
        for _ in 0..<500 where !done() { try await Task.sleep(for: .milliseconds(10)) }
        XCTAssertTrue(done(), file: file, line: line)
    }

    private func window(tick: TimeInterval = 1) -> HealthWindow {
        HealthWindow(
            read: { [unowned self] in
                reads += 1
                return health
            },
            hasAccess: { [unowned self] in fullDisk },
            rebuild: { [unowned self] in
                rebuilds += 1
                health.busy = true
            },
            stage: stage, folders: folders, format: HealthFormat(locale: Locale(identifier: "en_US")), tick: tick)
    }

    func testShowingReadsTheCoreAndTakesTheKeysOffStage() {
        let window = window()
        XCTAssertEqual(reads, 0, "nothing is read before it shows")
        window.show()
        defer { window.hide() }
        XCTAssertEqual(reads, 1)
        XCTAssertEqual(stage.forward, 1)
        XCTAssertTrue(stage.key === window.panel)
        XCTAssertTrue(window.panel.firstResponder === window.rebuild)
        XCTAssertEqual(window.entries.stringValue, "48,127")
        XCTAssertEqual(window.scanned.stringValue, "Just now, took 90 ms")
        XCTAssertNotNil(window.scanned.toolTip)
        XCTAssertEqual(window.changed.stringValue, "None since launch")
        XCTAssertNil(window.changed.toolTip)
        XCTAssertEqual(window.state.stringValue, "Up to date")
        XCTAssertEqual(window.access.stringValue, "On")
        XCTAssertTrue(window.grid.row(at: 5).isHidden, "no Open Settings with access")
        XCTAssertTrue(window.rebuild.isEnabled)
    }

    func testRebuildAsksTheCoreAndWaitsForTheNewList() {
        let window = window()
        window.show()
        defer { window.hide() }
        window.rebuild.performClick(nil)
        XCTAssertEqual(rebuilds, 1)
        XCTAssertEqual(window.state.stringValue, "Updating…")
        XCTAssertFalse(window.rebuild.isEnabled, "one rebuild at a time")

        health.busy = false
        health.changedAt = Date()
        window.refresh()
        XCTAssertTrue(window.rebuild.isEnabled)
        XCTAssertEqual(window.changed.stringValue, "Just now")
    }

    func testWithoutAccessOpenSettingsShowsAndOpensFullDiskAccess() {
        fullDisk = false
        let window = window()
        window.show()
        XCTAssertFalse(window.grid.row(at: 5).isHidden)
        XCTAssertTrue(window.access.stringValue.hasPrefix("Off"))
        let short = window.panel.frame
        window.settings.performClick(nil)
        XCTAssertFalse(window.shown)
        XCTAssertEqual(stage.opened, [Notice.needsAccess.settings])

        fullDisk = true
        window.show()
        defer { window.hide() }
        XCTAssertTrue(window.grid.row(at: 5).isHidden)
        XCTAssertLessThan(window.panel.frame.height, short.height)
        XCTAssertEqual(window.panel.frame.maxY, short.maxY, accuracy: 0.5, "the title bar stays put")
    }

    func testEscTheCloseButtonAndAnotherAppEachPutItAway() {
        let window = window()
        let ways: [(HealthWindow) -> Void] = [
            { $0.panel.cancelOperation(nil) },
            { XCTAssertFalse($0.windowShouldClose($0.panel)) },
            { $0.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification)) },
        ]
        for (at, away) in ways.enumerated() {
            window.show()
            away(window)
            XCTAssertFalse(window.shown, "\(at)")
            XCTAssertEqual(stage.away, at + 1)
        }
        window.hide()
        XCTAssertEqual(stage.away, ways.count, "hiding twice puts it away once")
    }

    func testItReadsOnlyWhileShownAndTicksOnTheirAges() async throws {
        let window = window(tick: 0.05)
        window.refresh()
        XCTAssertEqual(reads, 0)
        window.show()
        try await until { self.reads >= 3 }
        window.hide()
        let hidden = reads
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertEqual(reads, hidden, "hidden, it reads nothing")
    }

    func testTheAgesMoveOnWithTheClock() {
        let at = Date(timeIntervalSince1970: 1_800_000_000)
        health = Health(entries: 3, scannedAt: at, scanMs: 90, changedAt: at)
        let window = window()
        window.show()
        defer { window.hide() }
        window.refresh(now: at.addingTimeInterval(30))
        XCTAssertEqual(window.scanned.stringValue, "Just now, took 90 ms")
        XCTAssertEqual(window.changed.stringValue, "Just now")
        window.refresh(now: at.addingTimeInterval(3 * 60))
        XCTAssertEqual(window.scanned.stringValue, "3 minutes ago, took 90 ms")
        XCTAssertEqual(window.changed.stringValue, "3 minutes ago")
    }

    /// The first scan failed, the engine stopped, or it never started: no Rebuild to press.
    func testAStoppedCoreSaysSoAndOffersNoRebuild() {
        health = Health(stopped: true)
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.entries.stringValue, "0")
        XCTAssertEqual(window.scanned.stringValue, "Didn’t finish")
        XCTAssertEqual(window.state.stringValue, "Stopped: quit and reopen Telescope")
        XCTAssertFalse(window.rebuild.isEnabled)
    }

    func testAFailedRebuildSaysTheListIsBehindAndRebuildStaysOffered() {
        health.behind = true
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.state.stringValue, "Missing a change: rebuild the index")
        XCTAssertTrue(window.rebuild.isEnabled)
    }

    /// Each tick sets the same words again; only Open Settings coming or going resizes.
    func testATickWithNothingNewKeepsTheWindowsFrame() {
        let window = window()
        window.show()
        defer { window.hide() }
        let frame = window.panel.frame
        window.panel.setFrameOrigin(NSPoint(x: frame.minX + 40, y: frame.minY))
        window.refresh()
        XCTAssertEqual(window.panel.frame.minX, frame.minX + 40, "a window moved by hand stays")
        XCTAssertEqual(window.panel.frame.size, frame.size)
    }

    func testNoFolderIsSearchedInsideUntilYouAddOne() {
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.folderList.numberOfRows, 0)
        XCTAssertTrue(window.folderButtons.isEnabled(forSegment: 0))
        XCTAssertFalse(window.folderButtons.isEnabled(forSegment: 1), "nothing to remove")
    }

    func testAddingFoldersListsEachOnceAndKeepsTheWindowUp() {
        let home = NSHomeDirectory()
        let window = window()
        window.show()
        defer { window.hide() }
        stage.chosen = [URL(fileURLWithPath: home + "/notes"), URL(fileURLWithPath: "/Volumes/Work")]
        window.addFolders()
        XCTAssertEqual(stage.choosing, 1)
        XCTAssertEqual(folders.paths, [home + "/notes", "/Volumes/Work"])
        XCTAssertEqual(window.folderList.numberOfRows, 2)
        let first = window.folderList.view(atColumn: 0, row: 0, makeIfNecessary: true) as? NSTextField
        XCTAssertEqual(first?.stringValue, "~/notes")
        XCTAssertEqual(first?.toolTip, home + "/notes")
        XCTAssertTrue(stage.key === window.panel, "the keys come back once the open panel goes")
        XCTAssertTrue(window.shown)

        stage.chosen = [URL(fileURLWithPath: "/Volumes/Work")]
        window.addFolders()
        XCTAssertEqual(folders.paths.count, 2, "a folder chosen again is listed once")
        stage.chosen = []
        window.addFolders()
        XCTAssertEqual(folders.paths.count, 2, "Cancel adds nothing")
    }

    func testTheOpenPanelTakingTheKeysLeavesTheWindowUp() {
        let held = HeldStage()
        let holding = HealthWindow(
            read: { Health() }, hasAccess: { true }, rebuild: {}, stage: held, folders: folders)
        holding.show()
        defer { holding.hide() }
        holding.addFolders()
        XCTAssertTrue(holding.choosing)
        holding.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification))
        XCTAssertTrue(holding.shown, "the open panel took the keys")
        held.answer([URL(fileURLWithPath: "/tmp/notes")])
        XCTAssertFalse(holding.choosing)
        XCTAssertEqual(folders.paths, ["/tmp/notes"])
    }

    func testRemoveTakesTheSelectedFolders() {
        folders.add(["/a", "/b", "/c"])
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.folderList.numberOfRows, 3)
        window.folderList.selectRowIndexes([0, 2], byExtendingSelection: false)
        XCTAssertTrue(window.folderButtons.isEnabled(forSegment: 1))
        window.removeFolders()
        XCTAssertEqual(folders.paths, ["/b"])
        XCTAssertEqual(window.folderList.numberOfRows, 1)
        XCTAssertFalse(window.folderButtons.isEnabled(forSegment: 1))
    }
}

/// Holds the open panel's answer until the test gives it.
@MainActor
private final class HeldStage: Stage {
    private var done: (@MainActor ([URL]) -> Void)?
    var isActive = false
    var previewShows = false
    var previewIsKey = false
    func isKey(_ window: NSWindow) -> Bool { false }
    func bringForward(_ window: NSWindow) {}
    func putAway(_ window: NSWindow) {}
    func makeKey(_ window: NSWindow) {}
    func openPreview() {}
    func closePreview() {}
    func reloadPreview() {}
    func open(_ url: URL) {}
    func showInFinder(_ url: URL) {}
    func chooseFolders(_ done: @escaping @MainActor ([URL]) -> Void) { self.done = done }
    func answer(_ urls: [URL]) { done?(urls) }
}

@MainActor
final class TextFoldersTests: XCTestCase {
    func testFoldersAreKeptOnceInOrderAndAcrossLaunches() throws {
        let name = "telescope-tests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { defaults.removePersistentDomain(forName: name) }
        let folders = TextFolders(defaults: defaults)
        XCTAssertEqual(folders.paths, [], "none at first")
        folders.add(["/b", "/a", "/b"])
        folders.add(["/a", "/c"])
        XCTAssertEqual(folders.paths, ["/b", "/a", "/c"])
        XCTAssertEqual(TextFolders(defaults: defaults).paths, ["/b", "/a", "/c"])
        folders.remove(at: [0, 2, 7])
        XCTAssertEqual(folders.paths, ["/a"])
        XCTAssertEqual(TextFolders(defaults: defaults).paths, ["/a"])
        XCTAssertEqual(TextFolders().paths, [], "without defaults nothing is read")
    }

    func testASearchInsideAFolderReachesOnlyTheChosenFoldersThere() {
        let folders = TextFolders()
        folders.add(["/Users/me/notes", "/Users/me/code/app", "/Volumes/Work/"])
        XCTAssertEqual(folders.reached(within: nil), folders.paths)
        XCTAssertEqual(folders.reached(within: "/Users/me/notes/2026"), ["/Users/me/notes/2026"])
        XCTAssertEqual(folders.reached(within: "/Users/me/notes"), ["/Users/me/notes"])
        XCTAssertEqual(folders.reached(within: "/Users/me"), ["/Users/me/notes", "/Users/me/code/app"])
        XCTAssertEqual(folders.reached(within: "/Users/me/code"), ["/Users/me/code/app"])
        XCTAssertEqual(folders.reached(within: "/Users/me/notes2"), [], "a name that only starts the same")
        XCTAssertEqual(folders.reached(within: "/Users/me/downloads"), [])
        XCTAssertEqual(folders.reached(within: "/Volumes/Work/plans"), ["/Volumes/Work/plans"])
        XCTAssertEqual(folders.reached(within: "/"), folders.paths)
    }
}
