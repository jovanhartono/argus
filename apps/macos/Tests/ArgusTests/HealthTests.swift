import AppKit
import KeyboardShortcuts
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
        XCTAssertEqual(english.scan(opened, now: now), "Before Argus last started")
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
        XCTAssertEqual(english.state(Health(busy: true)), "Up to date", "a small patch says nothing")
        XCTAssertEqual(english.state(Health(busy: true, building: true)), "Building the index…")
        XCTAssertEqual(english.state(Health(behind: true)), "Missing a change: rebuild the index")
        XCTAssertEqual(english.state(Health(busy: true, behind: true)), "Missing a change: rebuild the index")
        XCTAssertEqual(
            english.state(Health(busy: true, building: true, behind: true)), "Building the index…", "the rebuild reads it")
        XCTAssertEqual(english.state(Health(stopped: true)), "Stopped: quit and reopen Argus")
        XCTAssertEqual(
            english.state(Health(busy: true, building: true, behind: true, stopped: true)),
            english.state(Health(stopped: true)))
        XCTAssertEqual(english.access(true), "On")
        XCTAssertEqual(english.access(false), "Off: Desktop, Documents, Downloads and external drives are left out")
        let everything = "Includes Library, clutter and your skipped folders, about 2.5M items."
        XCTAssertEqual(english.everything(true), everything)
        XCTAssertEqual(english.everything(false), everything + " Needs Full Disk Access.")
        let german = HealthFormat(locale: Locale(identifier: "de_DE"))
        XCTAssertTrue(german.everything(true).contains("about 2,5"), german.everything(true))
        let cloud = "iCloud Drive, Dropbox, Google Drive, OneDrive and others. Finding a file by name never downloads it."
        XCTAssertEqual(english.cloud(true), cloud)
        XCTAssertEqual(english.cloud(false), cloud + " Needs Full Disk Access.")
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
    private var cloudSwitched: [Bool] = []
    private var cloud = false

    override func setUp() async throws {
        _ = NSApplication.shared
        UserDefaults.keepHotkeysInMemory()
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
    private func window(
        tick: TimeInterval = 1, ignoreList: URL = HealthWindow.ignoreList, quit: @escaping () -> Void = {}
    ) -> HealthWindow {
        let (listed, cloudListed) = (everything, cloud)
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
            cloud: { [unowned self] in cloud },
            indexCloud: { [unowned self] on in
                cloudSwitched.append(on)
                cloud = on
                (health.busy, health.building) = (on != cloudListed, on != cloudListed)
            },
            ignoreList: ignoreList, quit: quit, format: HealthFormat(locale: Locale(identifier: "en_US")), tick: tick)
    }

    func testTheCloudSwitchIsOffAtFirstAndSpinsWhileTheNewListIsBuilt() {
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.cloud.title, "Include iCloud Drive and cloud storage")
        XCTAssertEqual(window.cloud.state, .off)
        XCTAssertTrue(window.cloud.isEnabled)
        XCTAssertFalse(window.cloudHint.stringValue.hasSuffix("Needs Full Disk Access."))

        window.cloud.performClick(nil)
        XCTAssertEqual(cloudSwitched, [true])
        XCTAssertEqual(switched, [], "the other switch stays as it was")
        XCTAssertFalse(window.progress.isHidden)
        XCTAssertFalse(window.rebuild.isEnabled)

        window.cloud.performClick(nil)
        XCTAssertEqual(cloudSwitched, [true, false])
        XCTAssertTrue(window.progress.isHidden, "nothing to build once switched back before it started")
        XCTAssertEqual(window.cloud.state, .off)
    }

    func testWithoutAccessTheCloudSwitchCanBeTurnedOffButNotOn() {
        fullDisk = false
        cloud = true
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.cloud.state, .on)
        XCTAssertTrue(window.cloud.isEnabled)
        XCTAssertTrue(window.cloudHint.stringValue.hasSuffix("Needs Full Disk Access."))
        window.cloud.performClick(nil)
        XCTAssertEqual(cloudSwitched, [false])
        XCTAssertFalse(window.cloud.isEnabled, "not on again until access comes")
        fullDisk = true
        window.refresh()
        XCTAssertTrue(window.cloud.isEnabled)
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
        XCTAssertEqual(window.state.stringValue, "Building the index…")
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
        XCTAssertEqual(window.state.stringValue, "Up to date")
    }

    /// Any whole rebuild spins it, not just Index everything, so it sits beside Status.
    func testTheSpinnerSitsBesideStatus() {
        let window = window()
        XCTAssertTrue(window.progress.superview === window.state.superview)
        XCTAssertFalse(window.everything.superview?.subviews.contains(window.progress) ?? true)
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
        XCTAssertFalse(window.everythingHint.stringValue.hasSuffix("Needs Full Disk Access."))
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
        XCTAssertFalse(window.cloud.isEnabled)
        XCTAssertTrue(window.progress.isHidden)
    }

    func testTabGoesTopToBottomAndRoundAgain() {
        let window = window()
        let order: [NSView] = [
            window.settings, window.rebuild, window.everything, window.cloud, window.folderList, window.folderButtons,
            window.editIgnore, window.windowKeys, window.finderKeys, window.openLoginItems, window.quit,
            window.settings,
        ]
        for (view, next) in zip(order, order.dropFirst()) {
            XCTAssertTrue(view.nextKeyView === next, "\(view) then \(next)")
        }
    }

    func testShowingReadsTheCoreAndTakesTheKeysOffStage() throws {
        let window = window()
        XCTAssertEqual(reads, 0, "nothing is read before it shows")
        window.show()
        defer { window.hide() }
        XCTAssertEqual(reads, 1)
        XCTAssertEqual(stage.forward, 1)
        XCTAssertTrue(stage.key === window.panel)
        XCTAssertEqual(window.panel.title, "Settings")
        let content = try XCTUnwrap(window.panel.contentView)
        XCTAssertEqual(content.frame.height, content.fittingSize.height, "Quit Argus at the bottom isn't cut off")
        XCTAssertTrue(window.panel.firstResponder === window.folderList, "Space mustn't start a rebuild")
        XCTAssertEqual(window.entries.stringValue, "48,127")
        XCTAssertEqual(window.scanned.stringValue, "Just now, took 90 ms")
        XCTAssertNotNil(window.scanned.toolTip)
        XCTAssertEqual(window.changed.stringValue, "None since launch")
        XCTAssertNil(window.changed.toolTip)
        XCTAssertEqual(window.state.stringValue, "Up to date")
        XCTAssertEqual(window.access.stringValue, "On")
        XCTAssertTrue(window.grid.row(at: 5).isHidden, "no Open System Settings with access")
        XCTAssertTrue(window.rebuild.isEnabled)
    }

    func testRebuildAsksTheCoreAndWaitsForTheNewList() {
        let window = window()
        window.show()
        defer { window.hide() }
        window.rebuild.performClick(nil)
        XCTAssertEqual(rebuilds, 1)
        XCTAssertEqual(window.state.stringValue, "Building the index…")
        XCTAssertFalse(window.rebuild.isEnabled, "one rebuild at a time")

        (health.busy, health.building) = (false, false)
        health.changedAt = Date()
        window.refresh()
        XCTAssertTrue(window.rebuild.isEnabled)
        XCTAssertEqual(window.changed.stringValue, "Just now")
    }

    func testWithoutAccessOpenSystemSettingsShowsAndOpensFullDiskAccess() {
        fullDisk = false
        let window = window()
        window.show()
        XCTAssertFalse(window.grid.row(at: 5).isHidden)
        XCTAssertEqual(window.settings.title, "Open System Settings", "not Argus's own Settings")
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

    /// Each tick sets the same words again; only Open System Settings coming or going resizes.
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

    func testAnEmptyListSaysNoFoldersYetUntilOneIsAdded() {
        let held = HeldStage()
        let empty = HealthWindow(
            read: { Health() }, hasAccess: { true }, rebuild: {}, stage: held, folders: folders)
        empty.show()
        defer { empty.hide() }
        XCTAssertFalse(empty.noFolders.isHidden)
        XCTAssertEqual(empty.noFolders.stringValue, "No folders yet")
        empty.addFolders()
        held.answer([URL(fileURLWithPath: "/tmp/notes")])
        XCTAssertTrue(empty.noFolders.isHidden, "a folder is listed")
        empty.folderList.selectRowIndexes([0], byExtendingSelection: false)
        empty.removeFolders()
        XCTAssertFalse(empty.noFolders.isHidden, "the last folder is gone")
        folders.add(["/a"])
        XCTAssertTrue(window().noFolders.isHidden, "a window opened over a kept list")
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

    func testDeleteAndForwardDeleteRemoveTheChosenFolders() {
        folders.add(["/a", "/b", "/c"])
        let window = window()
        window.show()
        defer { window.hide() }
        window.folderList.selectRowIndexes([1], byExtendingSelection: false)
        window.folderList.keyDown(with: key(NSDeleteCharacter))
        XCTAssertEqual(folders.paths, ["/a", "/c"])
        window.folderList.selectRowIndexes([0], byExtendingSelection: false)
        window.folderList.keyDown(with: key(NSDeleteFunctionKey))
        XCTAssertEqual(folders.paths, ["/c"])
        XCTAssertFalse(window.folderButtons.isEnabled(forSegment: 1))
    }

    func testAnEmptyListSaysSoInItsMiddleWithoutStripes() throws {
        let window = window()
        window.show()
        defer { window.hide() }
        window.panel.contentView?.layoutSubtreeIfNeeded()
        let list = try XCTUnwrap(window.noFolders.superview)
        XCTAssertFalse(list is NSScrollView, "a scroll view would place the note itself")
        XCTAssertEqual(window.noFolders.frame.midX, list.bounds.midX, accuracy: 1)
        XCTAssertEqual(window.noFolders.frame.midY, list.bounds.midY, accuracy: 1)
        XCTAssertFalse(window.folderList.usesAlternatingRowBackgroundColors)
        stage.chosen = [URL(fileURLWithPath: "/tmp/notes")]
        window.addFolders()
        XCTAssertTrue(window.folderList.usesAlternatingRowBackgroundColors)
    }

    func testEditMakesAnIgnoreListThatExplainsItselfOnceAndOpensIt() throws {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: home) }
        let list = home.appendingPathComponent(".config/argus/ignore")
        let window = window(ignoreList: list)
        window.show()
        XCTAssertEqual(window.editIgnore.title, "Edit…")
        window.editIgnore.performClick(nil)
        XCTAssertEqual(try String(contentsOf: list, encoding: .utf8), HealthWindow.ignoreExample)
        XCTAssertEqual(stage.openedAsText, [list])
        XCTAssertFalse(window.shown, "it steps aside for the editor")
        let rules = HealthWindow.ignoreExample.split(separator: "\n").filter { !$0.hasPrefix("#") }
        XCTAssertEqual(rules, [], "every example line is a note, so nothing is skipped until you write a rule")

        try Data("work\n".utf8).write(to: list)
        window.show()
        defer { window.hide() }
        window.editIgnore.performClick(nil)
        XCTAssertEqual(try String(contentsOf: list, encoding: .utf8), "work\n", "your own list is kept")
        XCTAssertEqual(stage.openedAsText, [list, list])
    }

    func testTheIgnoreListShowsWhereItLives() {
        XCTAssertEqual(HealthWindow.ignoreList.path, NSHomeDirectory() + "/.config/argus/ignore")
    }

    func testOpenLoginItemsOpensThatPageOfSystemSettings() {
        let window = window()
        window.show()
        window.openLoginItems.performClick(nil)
        XCTAssertFalse(window.shown)
        XCTAssertEqual(
            stage.opened.map(\.absoluteString), ["x-apple.systempreferences:com.apple.LoginItems-Settings.extension"])
    }

    func testQuitArgusQuits() {
        var quits = 0
        let window = window(quit: { quits += 1 })
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.quit.title, "Quit Argus")
        window.quit.performClick(nil)
        XCTAssertEqual(quits, 1)
    }

    /// Changing a hotkey is the way out when another app already uses ⌘⇧Space.
    func testBothHotkeysCanBeChangedHereButNothingRecordsAtOpen() {
        let window = window()
        window.show()
        defer { window.hide() }
        XCTAssertEqual(window.windowKeys.shortcutName, .toggleWindow)
        XCTAssertEqual(window.finderKeys.shortcutName, .searchFinderFolder)
        XCTAssertTrue(window.panel.firstResponder === window.folderList, "a recorder with the focus would take keys")
        XCTAssertNil(window.windowKeys.currentEditor())
        XCTAssertNil(window.finderKeys.currentEditor())
    }

    func testOneShortcutCanOnlyDoOneOfTheTwoJobs() {
        let window = window()
        let finders = KeyboardShortcuts.Shortcut(.space, modifiers: [.command, .shift, .option])
        let opens = KeyboardShortcuts.Shortcut(.space, modifiers: [.command, .shift])
        XCTAssertEqual(
            window.windowKeys.validateShortcut?(finders),
            .disallow(reason: "This shortcut is already used to search Finder’s folder."))
        XCTAssertEqual(
            window.finderKeys.validateShortcut?(opens), .disallow(reason: "This shortcut is already used to open Argus."))
        let free = KeyboardShortcuts.Shortcut(.k, modifiers: [.command, .option])
        XCTAssertEqual(window.windowKeys.validateShortcut?(free), .allow)
        XCTAssertEqual(window.windowKeys.validateShortcut?(opens), .allow, "its own keys again")
    }

    /// The warning a recorder shows, as for ⌘Space that Spotlight uses, is a sheet that takes the keys.
    func testARecordersWarningLeavesTheWindowUp() {
        let window = window()
        window.show()
        defer { window.hide() }
        window.windowWillBeginSheet(Notification(name: NSWindow.willBeginSheetNotification))
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification))
        XCTAssertTrue(window.shown)
        window.windowDidEndSheet(Notification(name: NSWindow.didEndSheetNotification))
        window.windowDidResignKey(Notification(name: NSWindow.didResignKeyNotification))
        XCTAssertFalse(window.shown, "with the warning gone, a click in another app hides it again")
    }

    /// A recorder lets go of the keys before passing Tab on, which used to send Tab back to the
    /// folder list, so Quit Argus and the rows above it couldn't be reached.
    func testTabGoesOnFromAHotkeyRecorder() async throws {
        let window = window()
        window.show()
        defer { window.hide() }
        // A recorder takes Tab only a moment after it joins a window, so it never starts with the focus.
        try await Task.sleep(for: .milliseconds(50))
        let tab = { (shift: Bool) in
            let character = shift ? "\u{19}" : "\t"
            return NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: shift ? [.shift] : [], timestamp: 0, windowNumber: 0,
                context: nil, characters: character, charactersIgnoringModifiers: character, isARepeat: false,
                keyCode: 48)!
        }
        for (shift, expected) in [(false, window.windowKeys.nextValidKeyView), (true, window.windowKeys.previousValidKeyView)] {
            XCTAssertTrue(window.panel.makeFirstResponder(window.windowKeys))
            XCTAssertTrue(window.panel.makeFirstResponder(nil), "what the recorder does with Tab")
            window.panel.keyDown(with: tab(shift))
            let reached = (window.panel.firstResponder as? NSText)?.delegate as? NSView ?? window.panel.firstResponder
            XCTAssertTrue(reached === expected, "\(String(describing: reached)) after \(shift ? "⇧Tab" : "Tab")")
        }
        XCTAssertTrue(window.windowKeys.nextValidKeyView === window.finderKeys, "the next recorder takes Tab")
        // Recording pauses every hotkey in the process, so none is left recording.
        window.panel.makeFirstResponder(nil)
        try await Task.sleep(for: .milliseconds(50))
    }

    /// 752 to 800 pt tall, it grew under the Dock on a 13" MacBook Air.
    func testTheWindowStaysAboveTheDock() throws {
        fullDisk = false
        let window = window()
        guard let visible = (window.panel.screen ?? NSScreen.main)?.visibleFrame, visible.height >= 800 else {
            throw XCTSkip("no screen tall enough to hold it")
        }
        window.panel.setFrameOrigin(NSPoint(x: visible.minX, y: visible.minY))
        window.show()
        defer { window.hide() }
        XCTAssertGreaterThan(window.panel.frame.height, 760, "it grew for the Open System Settings row")
        XCTAssertGreaterThanOrEqual(window.panel.frame.minY, visible.minY)
        XCTAssertLessThanOrEqual(window.panel.frame.maxY, visible.maxY)
    }

    private func key(_ code: Int) -> NSEvent {
        let characters = String(UnicodeScalar(UInt16(code)).map(Character.init)!)
        return NSEvent.keyEvent(
            with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0, windowNumber: 0, context: nil,
            characters: characters, charactersIgnoringModifiers: characters, isARepeat: false, keyCode: 0)!
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
    func openAsText(_ url: URL) {}
    func showInFinder(_ url: URL) {}
    func chooseFolders(on window: NSWindow, _ done: @escaping @MainActor ([URL]) -> Void) { self.done = done }
    func handBack() {}
    func answer(_ urls: [URL]) { done?(urls) }
}

@MainActor
final class IndexEverythingTests: XCTestCase {
    func testOffAtFirstAndKeptAcrossLaunches() throws {
        let defaults = MemoryDefaults()
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
        let defaults = MemoryDefaults()
        defaults.set("maybe", forKey: "indexEverything")
        XCTAssertFalse(IndexEverything(defaults: defaults).isOn)
        defaults.set("YES", forKey: "indexEverything")
        XCTAssertTrue(IndexEverything(defaults: defaults).isOn, "as `defaults write … -string YES` would")
    }
}

@MainActor
final class CloudDrivesTests: XCTestCase {
    func testOffAtFirstAndKeptAcrossLaunches() throws {
        let defaults = MemoryDefaults()
        let setting = CloudDrives(defaults: defaults)
        XCTAssertFalse(setting.isOn)
        setting.isOn = true
        XCTAssertTrue(CloudDrives(defaults: defaults).isOn)
        setting.isOn = false
        XCTAssertFalse(CloudDrives(defaults: defaults).isOn)
        let loose = CloudDrives()
        loose.isOn = true
        XCTAssertFalse(CloudDrives().isOn, "without defaults nothing is kept")
    }
}

@MainActor
final class TextFoldersTests: XCTestCase {
    func testFoldersAreKeptOnceInOrderAndAcrossLaunches() throws {
        let defaults = MemoryDefaults()
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
