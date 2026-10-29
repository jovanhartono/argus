import AppKit
import XCTest

@testable import Argus

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
        XCTAssertEqual(english.state(Health(stopped: true)), "Stopped: quit and reopen Argus")
        XCTAssertEqual(english.state(Health(busy: true, behind: true, stopped: true)), english.state(Health(stopped: true)))
        XCTAssertEqual(english.access(true), "On")
        XCTAssertEqual(english.access(false), "Off: Desktop, Documents, Downloads and cloud drives are left out")
        XCTAssertEqual(english.everything(true), "Includes Library and clutter, about 2.5M items.")
        XCTAssertEqual(
            english.everything(false), "Includes Library and clutter, about 2.5M items. Needs Full Disk Access.")
        let german = HealthFormat(locale: Locale(identifier: "de_DE"))
        XCTAssertTrue(german.everything(true).contains("about 2,5"), german.everything(true))
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
    private var switched: [Bool] = []
    private var everything = false

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

    /// The switch as the core takes it: switched back before the new list started, none is built.
    private func window(tick: TimeInterval = 1) -> HealthWindow {
        let listed = everything
        return HealthWindow(
            read: { [unowned self] in
                reads += 1
                return health
            },
            hasAccess: { [unowned self] in fullDisk },
            rebuild: { [unowned self] in
                rebuilds += 1
                (health.busy, health.building) = (true, true)
            },
            stage: stage, folders: folders, everything: { [unowned self] in everything },
            indexEverything: { [unowned self] on in
                switched.append(on)
                everything = on
                guard fullDisk else { return }
                (health.busy, health.building) = (on != listed, on != listed)
            },
            format: HealthFormat(locale: Locale(identifier: "en_US")), tick: tick)
    }

    func testIndexEverythingSpinsWhileTheNewListIsBuiltAndStopsWhenSwitchedBackInTime() {
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.everything.state, .off)
        XCTAssertTrue(window.everything.isEnabled)
        XCTAssertTrue(window.progress.isHidden, "nothing is being built")
        XCTAssertEqual(window.progress.accessibilityLabel(), "Building the index")

        window.everything.performClick(nil)
        XCTAssertEqual(switched, [true])
        XCTAssertFalse(window.progress.isHidden)
        XCTAssertEqual(window.state.stringValue, "Updating…")
        XCTAssertFalse(window.rebuild.isEnabled)
        XCTAssertTrue(window.everything.isEnabled, "it can be switched back")

        window.everything.performClick(nil)
        XCTAssertEqual(switched, [true, false])
        XCTAssertTrue(window.progress.isHidden, "nothing to build once switched back before it started")
        XCTAssertEqual(window.state.stringValue, "Up to date")
        XCTAssertTrue(window.rebuild.isEnabled)
    }

    func testTheSwitchShowsTheSettingAsItIsNow() {
        everything = true
        let window = window()
        window.show()
        XCTAssertEqual(window.everything.state, .on)
        window.hide()
        everything = false
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.everything.state, .off, "set elsewhere while hidden")
    }

    /// With everything listed, ~/Library changes every few seconds and each change is a patch.
    func testAPatchNeitherSpinsNorTakesRebuildAway() {
        health.busy = true
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertTrue(window.progress.isHidden)
        XCTAssertTrue(window.rebuild.isEnabled)
        XCTAssertEqual(window.state.stringValue, "Updating…")
    }

    /// Reading all of ~/Library without access stops on a macOS pop-up for each media library.
    func testWithoutAccessTheSwitchWaitsAndSaysWhy() {
        fullDisk = false
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertFalse(window.everything.isEnabled, "can't be turned on yet")
        XCTAssertTrue(window.everythingHint.stringValue.hasSuffix("Needs Full Disk Access."))

        fullDisk = true
        window.refresh()
        XCTAssertTrue(window.everything.isEnabled)
        XCTAssertEqual(window.everythingHint.stringValue, "Includes Library and clutter, about 2.5M items.")
    }

    /// Kept on from before access was taken away, or from another Mac.
    func testWithoutAccessASwitchLeftOnCanStillBeTurnedOff() {
        fullDisk = false
        everything = true
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.everything.state, .on)
        XCTAssertTrue(window.everything.isEnabled)
        window.everything.performClick(nil)
        XCTAssertEqual(switched, [false])
        XCTAssertEqual(window.everything.state, .off)
        XCTAssertFalse(window.everything.isEnabled, "and not on again until access comes")
        XCTAssertTrue(window.progress.isHidden)
    }

    func testAStoppedCoreTakesNoSwitchAndShowsNoSpinner() {
        health = Health(busy: true, building: true, stopped: true)
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertFalse(window.everything.isEnabled)
        XCTAssertTrue(window.progress.isHidden)
    }

    func testTabReachesTheSwitch() {
        let window = window()
        XCTAssertTrue(window.settings.nextKeyView === window.everything)
        XCTAssertTrue(window.everything.nextKeyView === window.folderList)
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

        (health.busy, health.building) = (false, false)
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
        XCTAssertEqual(window.state.stringValue, "Stopped: quit and reopen Argus")
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
        stage.key = nil
        window.addFolders()
        XCTAssertEqual(stage.choosing, 1)
        XCTAssertTrue(stage.sheetOn === window.panel, "the open panel hangs from this window, never behind it")
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
        holding.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification))
        XCTAssertFalse(holding.shown, "with the open panel gone, a click in another app hides it again")
    }

    func testHidingHandsTheKeysBack() {
        let window = window()
        window.show()
        XCTAssertEqual(stage.handedBack, 0)
        window.hide()
        XCTAssertEqual(stage.handedBack, 1)
        window.hide()
        XCTAssertEqual(stage.handedBack, 1, "only when it was showing")
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
    func chooseFolders(on window: NSWindow, _ done: @escaping @MainActor ([URL]) -> Void) { self.done = done }
    func handBack() {}
    func answer(_ urls: [URL]) { done?(urls) }
}

@MainActor
final class IndexEverythingTests: XCTestCase {
    func testOffAtFirstAndKeptAcrossLaunches() throws {
        let name = "argus-tests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { defaults.removePersistentDomain(forName: name) }
        let setting = IndexEverything(defaults: defaults)
        XCTAssertFalse(setting.isOn)
        setting.isOn = true
        XCTAssertTrue(IndexEverything(defaults: defaults).isOn)
        setting.isOn = false
        XCTAssertFalse(IndexEverything(defaults: defaults).isOn)
        let loose = IndexEverything()
        loose.isOn = true
        XCTAssertFalse(IndexEverything().isOn, "without defaults nothing is kept")
    }

    func testAValueThatIsNoBooleanReadsAsOff() throws {
        let name = "argus-tests-\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: name))
        defer { defaults.removePersistentDomain(forName: name) }
        defaults.set("maybe", forKey: "indexEverything")
        XCTAssertFalse(IndexEverything(defaults: defaults).isOn)
        defaults.set("YES", forKey: "indexEverything")
        XCTAssertTrue(IndexEverything(defaults: defaults).isOn, "as `defaults write … -string YES` would")
    }
}

@MainActor
final class TextFoldersTests: XCTestCase {
    func testFoldersAreKeptOnceInOrderAndAcrossLaunches() throws {
        let name = "argus-tests-\(UUID().uuidString)"
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

    func testAFolderChosenThroughALinkIsKeptAtItsRealPlace() throws {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let notes = home.appendingPathComponent("notes")
        try FileManager.default.createDirectory(at: notes, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: home) }
        let link = home.appendingPathComponent("link")
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: notes)
        let real = String(cString: realpath(notes.path, nil))
        XCTAssertTrue(notes.path.hasPrefix("/var/"), "the temporary folder is reached through /var")
        let folders = TextFolders()
        folders.add([notes.path, link.path])
        XCTAssertEqual(folders.paths, [real], "both name the same folder")
        XCTAssertEqual(folders.reached(within: real + "/2026"), [real + "/2026"])
    }
}
