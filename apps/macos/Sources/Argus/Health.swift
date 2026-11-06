import AppKit
import KeyboardShortcuts
import ServiceManagement

/// How the list in use is doing, as `tl_health` tells it.
struct Health: Equatable {
    var entries: UInt64 = 0
    /// Nil while the list in use is the one saved at the last launch.
    var scannedAt: Date?
    var scanMs: UInt64 = 0
    /// Nil until a list after the first comes into use.
    var changedAt: Date?
    var busy = false
    /// Busy reading the whole folder, not just the folders that changed.
    var building = false
    /// A rebuild failed, so the list misses a change until a whole new one reads it.
    var behind = false
    /// Search stopped working, or never started, and no list will come.
    var stopped = false
}

/// The health view's words, in your language where macOS has them.
struct HealthFormat {
    var locale = Locale.autoupdatingCurrent

    func entries(_ health: Health) -> String {
        health.entries.formatted(.number.locale(locale))
    }

    /// "5 minutes ago, took 0.9 s".
    func scan(_ health: Health, now: Date) -> String {
        guard let at = health.scannedAt else {
            if health.entries == 0 && health.stopped { return "Didn’t finish" }
            return health.busy && health.entries == 0 ? "Under way" : "Before Argus last started"
        }
        return "\(ago(at, now: now)), took \(duration(ms: health.scanMs))"
    }

    func change(_ health: Health, now: Date) -> String {
        health.changedAt.map { ago($0, now: now) } ?? "None since launch"
    }

    func state(_ health: Health) -> String {
        if health.stopped { return "Stopped: press ⌘Q, then open Argus again" }
        // With Index everything on, ~/Library changes every few seconds; those small patches
        // aren't worth a word.
        if health.building { return "Building the index…" }
        return health.behind ? "Missing a change: rebuild the index" : "Up to date"
    }

    func access(_ fullDisk: Bool) -> String {
        fullDisk ? "On" : "Off: Desktop, Documents, Downloads and external drives are left out"
    }

    /// A whole home folder held 2,478,045 entries on the Mac this was measured on.
    func everything(_ fullDisk: Bool) -> String {
        let count = 2_500_000.formatted(.number.notation(.compactName).locale(locale))
        let what = "Includes Library, clutter and your skipped folders, about \(count) items."
        return fullDisk ? what : what + " Needs Full Disk Access."
    }

    func cloud(_ fullDisk: Bool) -> String {
        let what = "iCloud Drive, Dropbox, Google Drive, OneDrive and others. Finding a file by name never downloads it."
        return fullDisk ? what : what + " Needs Full Disk Access."
    }

    func login(_ status: SMAppService.Status, byHand: Bool) -> String {
        if byHand { return "Added in System Settings. Click to open Login Items and change it there." }
        return status == .requiresApproval
            ? "Turned off in System Settings. Click to open Login Items and turn it back on."
            : "At login Argus stays out of sight until you press the hotkey."
    }

    /// Under a minute, or a clock set back, reads "Just now".
    func ago(_ date: Date, now: Date) -> String {
        guard now.timeIntervalSince(date) >= 60 else { return "Just now" }
        let words = RelativeDateTimeFormatter()
        words.locale = locale
        words.unitsStyle = .full
        words.dateTimeStyle = .named
        words.formattingContext = .beginningOfSentence
        return words.localizedString(for: date, relativeTo: now)
    }

    /// "120 ms", "1.5 s", "2 min 5 s".
    func duration(ms: UInt64) -> String {
        if ms < 1_000 { return "\(ms) ms" }
        if ms < 60_000 {
            let seconds = Double(ms) / 1_000
            return seconds.formatted(.number.precision(.fractionLength(1)).locale(locale)) + " s"
        }
        let seconds = ms / 1_000
        return "\(seconds / 60) min \(seconds % 60) s"
    }
}

/// Esc closes it, as it does the search window.
final class HealthPanel: NSPanel {
    override var canBecomeKey: Bool { true }
    /// A hotkey recorder lets go of the keys before passing Tab on, so AppKit would send Tab back
    /// to the first row and the rows under the recorders could never be reached.
    private weak var recorderLeft: NSView?

    override func cancelOperation(_ sender: Any?) {
        (delegate as? HealthWindow)?.hide()
    }

    @discardableResult
    override func makeFirstResponder(_ responder: NSResponder?) -> Bool {
        let recorder = (firstResponder as? NSText)?.delegate as? KeyboardShortcuts.RecorderCocoa
        let made = super.makeFirstResponder(responder)
        if made { recorderLeft = responder == nil ? recorder : nil }
        if made, let view = responder as? NSView { reveal(view) }
        return made
    }

    /// On a short screen the window scrolls, and Tab can land on a row below the bottom edge.
    private func reveal(_ view: NSView) {
        guard let page = (contentView as? NSScrollView)?.documentView, view.isDescendant(of: page) else { return }
        page.scrollToVisible(view.convert(view.bounds, to: page))
    }

    override func selectNextKeyView(_ sender: Any?) {
        guard firstResponder === self, let recorderLeft else { return super.selectNextKeyView(sender) }
        selectKeyView(following: recorderLeft)
    }

    override func selectPreviousKeyView(_ sender: Any?) {
        guard firstResponder === self, let recorderLeft else { return super.selectPreviousKeyView(sender) }
        selectKeyView(preceding: recorderLeft)
    }
}

/// Settings' rows, laid from the top so a short window shows Entries first and scrolls down.
final class SettingsPage: NSView {
    override var isFlipped: Bool { true }
}

/// ⌘,'s Settings window: how many entries the list holds, when it was last read in full and
/// last changed, whether Argus may read every folder, a button to build the list again, the
/// switches to list everything and cloud drives, the ignore list, both hotkeys, Login Items
/// and Quit.
@MainActor
final class HealthWindow: NSObject, NSWindowDelegate {
    static let loginItems = URL(string: "x-apple.systempreferences:com.apple.LoginItems-Settings.extension")!
    /// The core reads it from this place in your home folder (argus-core's ignore.rs).
    static let ignoreList = URL(fileURLWithPath: NSHomeDirectory() + "/.config/argus/ignore")
    static let ignoreExample = """
        # Folders Argus leaves out of search, one per line. Lines starting with # are notes.
        #
        # A name leaves out every folder with that name:
        # build
        #
        # A path leaves out that one folder:
        # ~/Downloads/old
        #
        # ! brings back a folder Argus leaves out:
        # !~/.config

        """

    let panel = HealthPanel(
        contentRect: NSRect(x: 0, y: 0, width: 460, height: 200),
        styleMask: [.titled, .closable, .nonactivatingPanel], backing: .buffered, defer: true)
    let entries = NSTextField(labelWithString: "")
    let scanned = NSTextField(labelWithString: "")
    let changed = NSTextField(labelWithString: "")
    let access = NSTextField(wrappingLabelWithString: "")
    let state = NSTextField(labelWithString: "")
    let settings = KeyButton(title: "Open System Settings", target: nil, action: nil)
    let rebuild = KeyButton(title: "Rebuild Index", target: nil, action: nil)
    let everything = NSButton(checkboxWithTitle: "Index everything", target: nil, action: nil)
    /// Spins beside Status while a whole new list is built, as after flipping a switch: 10 to 20 s.
    let progress = NSProgressIndicator()
    let everythingHint = NSTextField(wrappingLabelWithString: "")
    let cloud = NSButton(checkboxWithTitle: "Include iCloud Drive and cloud storage", target: nil, action: nil)
    let cloudHint = NSTextField(wrappingLabelWithString: "")
    let grid = NSGridView()
    let scroll = NSScrollView()
    let page = SettingsPage()
    let editIgnore = KeyButton(title: "Edit…", target: nil, action: nil)
    let windowKeys = KeyboardShortcuts.RecorderCocoa(for: .toggleWindow)
    let finderKeys = KeyboardShortcuts.RecorderCocoa(for: .searchFinderFolder)
    let startAtLogin = NSButton(checkboxWithTitle: "Open Argus when you log in", target: nil, action: nil)
    let loginHint = NSTextField(wrappingLabelWithString: "")
    let quit = KeyButton(title: "Quit Argus", target: nil, action: nil)
    private var sheetUp = false
    private let read: () -> Health
    private let hasAccess: () -> Bool
    private let onRebuild: () -> Void
    private let isEverything: () -> Bool
    private let onEverything: (Bool) -> Void
    private let isCloud: () -> Bool
    private let onCloud: (Bool) -> Void
    private let stage: any Stage
    private let loginItems: any LoginItems
    /// Whether System Settings' Open at Login list has Argus, added by hand or not.
    private let listedAtLogin: () -> Bool
    private var byHand = false
    private let ignoreList: URL
    private let onQuit: () -> Void
    private let format: HealthFormat
    private let tick: TimeInterval
    /// The screen under the menu bar and above the Dock.
    private let usableArea: @MainActor (NSWindow) -> NSRect?
    private let dates = DetailsFormat()
    private var ticking: Timer?
    private var fittedTo = NSRect.null
    private(set) var shown = false
    private(set) var health = Health()

    init(
        read: @escaping () -> Health, hasAccess: @escaping () -> Bool, rebuild: @escaping () -> Void,
        stage: any Stage, everything: @escaping () -> Bool = { false },
        indexEverything: @escaping (Bool) -> Void = { _ in }, cloud: @escaping () -> Bool = { false },
        indexCloud: @escaping (Bool) -> Void = { _ in }, loginItems: any LoginItems = MemoryLoginItems(),
        listedAtLogin: @escaping () -> Bool = { false }, ignoreList: URL = HealthWindow.ignoreList,
        quit: @escaping () -> Void = { NSApp.terminate(nil) }, format: HealthFormat = HealthFormat(),
        tick: TimeInterval = 1,
        usableArea: @escaping @MainActor (NSWindow) -> NSRect? = { ($0.screen ?? NSScreen.main)?.visibleFrame }
    ) {
        self.usableArea = usableArea
        self.read = read
        self.hasAccess = hasAccess
        onRebuild = rebuild
        isEverything = everything
        onEverything = indexEverything
        isCloud = cloud
        onCloud = indexCloud
        self.stage = stage
        self.loginItems = loginItems
        self.listedAtLogin = listedAtLogin
        self.ignoreList = ignoreList
        onQuit = quit
        self.format = format
        self.tick = tick
        super.init()
        build()
    }

    private func build() {
        let label = { (text: String) -> NSView in NSTextField(labelWithString: text) }
        let status = NSStackView(views: [state, progress])
        status.spacing = 6
        let ignorePath = NSTextField(labelWithString: (ignoreList.path as NSString).abbreviatingWithTildeInPath)
        ignorePath.lineBreakMode = .byTruncatingMiddle
        let ignoreLine = NSStackView(views: [ignorePath, editIgnore])
        ignoreLine.spacing = 8
        let rows: [[NSView]] = [
            [label("Entries:"), entries],
            [label("Last full scan:"), scanned],
            [label("Last change applied:"), changed],
            [label("Status:"), status],
            [label("Full Disk Access:"), access],
            [NSGridCell.emptyContentView, settings],
            [NSGridCell.emptyContentView, rebuild],
            [NSGridCell.emptyContentView, everythingBox()],
            [NSGridCell.emptyContentView, cloudBox()],
            [label("Skipped folders:"), ignoreLine],
            [label("Open Argus:"), windowKeys],
            [label("Search Finder’s folder:"), finderKeys],
            [label("Start at login:"), loginBox()],
            [NSGridCell.emptyContentView, quit],
        ]
        for row in rows { grid.addRow(with: row) }
        for row in [7, 9, 10, 12, 13] { grid.row(at: row).topPadding = 12 }
        panel.title = "Settings"
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.hidesOnDeactivate = false
        panel.isReleasedWhenClosed = false
        panel.delegate = self
        access.preferredMaxLayoutWidth = 260
        for button in [settings, rebuild, editIgnore, quit] {
            button.target = self
            button.bezelStyle = .push
        }
        settings.action = #selector(openSettings(_:))
        rebuild.action = #selector(rebuildIndex(_:))
        editIgnore.action = #selector(editIgnoreList(_:))
        quit.action = #selector(quitArgus(_:))
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 8
        grid.columnSpacing = 8
        grid.row(at: 6).topPadding = 12
        grid.translatesAutoresizingMaskIntoConstraints = false
        page.addSubview(grid)
        page.translatesAutoresizingMaskIntoConstraints = false
        scroll.documentView = page
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        NSLayoutConstraint.activate([
            grid.topAnchor.constraint(equalTo: page.topAnchor, constant: 20),
            grid.bottomAnchor.constraint(equalTo: page.bottomAnchor, constant: -20),
            grid.leadingAnchor.constraint(equalTo: page.leadingAnchor, constant: 20),
            grid.trailingAnchor.constraint(lessThanOrEqualTo: page.trailingAnchor, constant: -20),
            page.widthAnchor.constraint(equalToConstant: 460),
            page.topAnchor.constraint(equalTo: scroll.contentView.topAnchor),
            page.leadingAnchor.constraint(equalTo: scroll.contentView.leadingAnchor),
        ])
        panel.contentView = scroll
        let order: [NSView] = [
            settings, rebuild, everything, cloud, editIgnore, windowKeys, finderKeys, startAtLogin, quit,
        ]
        for (at, view) in order.enumerated() { view.nextKeyView = order[(at + 1) % order.count] }
        // One press would run both, and the search window would flash up and straight away.
        let uses: [(KeyboardShortcuts.RecorderCocoa, KeyboardShortcuts.Name, String)] = [
            (windowKeys, .searchFinderFolder, "search Finder’s folder"), (finderKeys, .toggleWindow, "open Argus"),
        ]
        for (recorder, other, use) in uses {
            recorder.validateShortcut = { shortcut in
                shortcut == KeyboardShortcuts.getShortcut(for: other)
                    ? .disallow(reason: "This shortcut is already used to \(use).") : .allow
            }
        }
        panel.setContentSize(page.fittingSize)
        panel.center()
    }

    private func everythingBox() -> NSView {
        everything.target = self
        everything.action = #selector(switchEverything(_:))
        progress.style = .spinning
        progress.controlSize = .small
        progress.isHidden = true
        progress.setAccessibilityLabel("Building the index")
        everythingHint.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        everythingHint.textColor = .secondaryLabelColor
        everythingHint.preferredMaxLayoutWidth = 260
        let box = NSStackView(views: [everything, everythingHint])
        box.orientation = .vertical
        box.alignment = .leading
        box.spacing = 2
        return box
    }

    @objc func switchEverything(_ sender: Any?) {
        onEverything(everything.state == .on)
        refresh()
    }

    private func cloudBox() -> NSView {
        cloud.target = self
        cloud.action = #selector(switchCloud(_:))
        cloudHint.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        cloudHint.textColor = .secondaryLabelColor
        cloudHint.preferredMaxLayoutWidth = 260
        let box = NSStackView(views: [cloud, cloudHint])
        box.orientation = .vertical
        box.alignment = .leading
        box.spacing = 2
        return box
    }

    @objc func switchCloud(_ sender: Any?) {
        onCloud(cloud.state == .on)
        refresh()
    }

    private func loginBox() -> NSView {
        startAtLogin.target = self
        startAtLogin.action = #selector(switchLogin(_:))
        loginHint.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        loginHint.textColor = .secondaryLabelColor
        loginHint.preferredMaxLayoutWidth = 260
        let box = NSStackView(views: [startAtLogin, loginHint])
        box.orientation = .vertical
        box.alignment = .leading
        box.spacing = 2
        return box
    }

    /// Once turned off in System Settings, only System Settings can turn it back on, and an
    /// entry added there by hand is out of Argus's reach.
    @objc func switchLogin(_ sender: Any?) {
        if loginItems.status == .requiresApproval || byHand {
            hide()
            stage.open(Self.loginItems)
            return
        }
        // A refusal from macOS shows as the switch going back to what it was.
        try? startAtLogin.state == .on ? loginItems.register() : loginItems.unregister()
        refresh()
    }

    func show() {
        shown = true
        // Read once per show, not every tick: the old list takes about 44 ms.
        byHand = loginItems.status != .enabled && listedAtLogin()
        refresh()
        // A new display resolution or another screen since the last show would leave Quit Argus
        // below the edge; measuring the rows takes about 6 ms, so only then.
        if (usableArea(panel) ?? .zero) != fittedTo { fitKeepingTop() }
        stage.bringForward(panel)
        stage.makeKey(panel)
        // No row has the keys at first, so Space does nothing; on Rebuild Index it started a
        // 10 to 20 s rebuild.
        panel.makeFirstResponder(nil)
        // The ages tick on while it shows, and a rebuild with no new list still ends.
        ticking?.invalidate()
        ticking = Timer.scheduledTimer(withTimeInterval: tick, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.refresh() }
        }
        ticking?.tolerance = tick / 2
    }

    func hide() {
        guard shown else { return }
        shown = false
        ticking?.invalidate()
        ticking = nil
        stage.putAway(panel)
    }

    /// Reads the core again; does nothing while hidden, so a hidden window costs nothing.
    func refresh(now: Date = Date()) {
        guard shown else { return }
        health = read()
        let fullDisk = hasAccess()
        entries.stringValue = format.entries(health)
        scanned.stringValue = format.scan(health, now: now)
        scanned.toolTip = health.scannedAt.map(dates.date)
        changed.stringValue = format.change(health, now: now)
        changed.toolTip = health.changedAt.map(dates.date)
        state.stringValue = format.state(health)
        access.stringValue = format.access(fullDisk)
        let building = health.building && !health.stopped
        rebuild.isEnabled = !building && !health.stopped
        let on = isEverything()
        everything.state = on ? .on : .off
        // Without access it can still be turned off, just not on.
        everything.isEnabled = (fullDisk || on) && !health.stopped
        everythingHint.stringValue = format.everything(fullDisk)
        let cloudOn = isCloud()
        cloud.state = cloudOn ? .on : .off
        // Without access cloud drives stay unread, so it can be turned off but not on.
        cloud.isEnabled = (fullDisk || cloudOn) && !health.stopped
        cloudHint.stringValue = format.cloud(fullDisk)
        let login = loginItems.status
        startAtLogin.state = login == .enabled || byHand ? .on : .off
        loginHint.stringValue = format.login(login, byHand: byHand)
        if building == progress.isHidden {
            progress.isHidden = !building
            building ? progress.startAnimation(nil) : progress.stopAnimation(nil)
            let words = building ? "Building the index" : "Index built"
            NSAccessibility.post(
                element: progress, notification: .announcementRequested,
                userInfo: [.announcement: words, .priority: NSAccessibilityPriorityLevel.high.rawValue])
        }
        guard grid.row(at: 5).isHidden != fullDisk else { return }
        grid.row(at: 5).isHidden = fullDisk
        fitKeepingTop()
    }

    /// The Open System Settings row comes and goes; the title bar stays where it was.
    private func fitKeepingTop() {
        let rows = page.fittingSize
        var frame = panel.frameRect(forContentRect: NSRect(origin: .zero, size: rows))
        // On a screen that looks like 1280 × 800 the whole window is taller than the room under
        // the menu bar, so it takes that room and the rows scroll.
        let visible = usableArea(panel)
        fittedTo = visible ?? .zero
        if let visible, frame.height > visible.height {
            // With scroll bars set to show always, the scroller takes room beside the rows.
            let content = NSScrollView.frameSize(
                forContentSize: rows, horizontalScrollerClass: nil, verticalScrollerClass: NSScroller.self,
                borderType: .noBorder, controlSize: .regular, scrollerStyle: scroll.scrollerStyle)
            frame.size = NSSize(
                width: panel.frameRect(forContentRect: NSRect(origin: .zero, size: content)).width,
                height: visible.height)
        }
        frame.origin = NSPoint(x: panel.frame.minX, y: panel.frame.maxY - frame.height)
        // Grown downward on a 13" MacBook Air, Quit Argus ended up under the Dock.
        if let visible {
            frame.origin.y = min(max(frame.minY, visible.minY), visible.maxY - frame.height)
        }
        if frame != panel.frame { panel.setFrame(frame, display: shown) }
    }

    @objc func rebuildIndex(_ sender: Any?) {
        onRebuild()
        refresh()
    }

    @objc private func openSettings(_ sender: Any?) {
        hide()
        stage.open(Notice.needsAccess.settings)
    }

    /// A first-time user has no list yet, so they get one that explains itself.
    @objc func editIgnoreList(_ sender: Any?) {
        let files = FileManager.default
        if !files.fileExists(atPath: ignoreList.path) {
            try? files.createDirectory(at: ignoreList.deletingLastPathComponent(), withIntermediateDirectories: true)
            try? Data(Self.ignoreExample.utf8).write(to: ignoreList)
        }
        hide()
        stage.openAsText(ignoreList)
    }

    /// With no Dock icon or menu bar icon, this and ⌘Q are the only ways to quit.
    @objc func quitArgus(_ sender: Any?) {
        onQuit()
    }

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        hide()
        return false
    }

    /// A recorder's warning, as for ⌘Space taken by Spotlight, is a sheet that takes the keys;
    /// hiding under it left the window stuck on screen.
    func windowWillBeginSheet(_ note: Notification) {
        sheetUp = true
    }

    func windowDidEndSheet(_ note: Notification) {
        sheetUp = false
    }

    /// Like the search window, it goes once you click in another app.
    func windowDidResignKey(_ note: Notification) {
        guard !sheetUp else { return }
        hide()
    }
}
