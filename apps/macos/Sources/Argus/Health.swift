import AppKit
import KeyboardShortcuts

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
    /// to the folder list and the rows under the recorders could never be reached.
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
        // The folder list scrolls inside its own box; that box is what has to be in view.
        let shown = view.enclosingScrollView.flatMap { $0.isDescendant(of: page) ? $0 : nil } ?? view
        page.scrollToVisible(shown.convert(shown.bounds, to: page))
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

/// Delete removes the chosen folders, as in any macOS list with + and − under it.
final class FolderList: NSTableView {
    var onDelete: () -> Void = {}

    override func keyDown(with event: NSEvent) {
        let delete = event.specialKey == .delete || event.specialKey == .deleteForward
        guard delete, !selectedRowIndexes.isEmpty else { return super.keyDown(with: event) }
        onDelete()
    }
}

/// ⌘,'s Settings window: how many entries the list holds, when it was last read in full and
/// last changed, whether Argus may read every folder, a button to build the list again, the
/// switches to list everything and cloud drives, the folders whose files' text a search looks
/// inside, the ignore list, both hotkeys, Login Items and Quit.
@MainActor
final class HealthWindow: NSObject, NSWindowDelegate, NSTableViewDataSource, NSTableViewDelegate {
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
    let folderList = FolderList()
    let noFolders = NSTextField(labelWithString: "No folders yet")
    let editIgnore = KeyButton(title: "Edit…", target: nil, action: nil)
    let windowKeys = KeyboardShortcuts.RecorderCocoa(for: .toggleWindow)
    let finderKeys = KeyboardShortcuts.RecorderCocoa(for: .searchFinderFolder)
    let openLoginItems = KeyButton(title: "Open Login Items…", target: nil, action: nil)
    let quit = KeyButton(title: "Quit Argus", target: nil, action: nil)
    let folderButtons = NSSegmentedControl(
        images: [NSImage(named: NSImage.addTemplateName)!, NSImage(named: NSImage.removeTemplateName)!],
        trackingMode: .momentary, target: nil, action: nil)
    let folders: TextFolders
    /// The open panel takes the keys meanwhile, which must not close this window.
    private(set) var choosing = false
    private var sheetUp = false
    private let read: () -> Health
    private let hasAccess: () -> Bool
    private let onRebuild: () -> Void
    private let isEverything: () -> Bool
    private let onEverything: (Bool) -> Void
    private let isCloud: () -> Bool
    private let onCloud: (Bool) -> Void
    private let stage: any Stage
    private let ignoreList: URL
    private let onQuit: () -> Void
    private let format: HealthFormat
    private let tick: TimeInterval
    /// The screen under the menu bar and above the Dock.
    private let usableArea: @MainActor (NSWindow) -> NSRect?
    private let dates = DetailsFormat()
    private var ticking: Timer?
    private var fitted = false
    private(set) var shown = false
    private(set) var health = Health()

    init(
        read: @escaping () -> Health, hasAccess: @escaping () -> Bool, rebuild: @escaping () -> Void,
        stage: any Stage, folders: TextFolders = TextFolders(), everything: @escaping () -> Bool = { false },
        indexEverything: @escaping (Bool) -> Void = { _ in }, cloud: @escaping () -> Bool = { false },
        indexCloud: @escaping (Bool) -> Void = { _ in }, ignoreList: URL = HealthWindow.ignoreList,
        quit: @escaping () -> Void = { NSApp.terminate(nil) }, format: HealthFormat = HealthFormat(),
        tick: TimeInterval = 1,
        usableArea: @escaping @MainActor (NSWindow) -> NSRect? = { ($0.screen ?? NSScreen.main)?.visibleFrame }
    ) {
        self.usableArea = usableArea
        self.folders = folders
        self.read = read
        self.hasAccess = hasAccess
        onRebuild = rebuild
        isEverything = everything
        onEverything = indexEverything
        isCloud = cloud
        onCloud = indexCloud
        self.stage = stage
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
            [label("Search inside files in:"), folderBox()],
            [label("Skipped folders:"), ignoreLine],
            [label("Open Argus:"), windowKeys],
            [label("Search Finder’s folder:"), finderKeys],
            [label("Start at login:"), openLoginItems],
            [NSGridCell.emptyContentView, quit],
        ]
        for row in rows { grid.addRow(with: row) }
        for row in [7, 9, 10, 11, 13, 14] { grid.row(at: row).topPadding = 12 }
        panel.title = "Settings"
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.hidesOnDeactivate = false
        panel.isReleasedWhenClosed = false
        panel.delegate = self
        access.preferredMaxLayoutWidth = 260
        for button in [settings, rebuild, editIgnore, openLoginItems, quit] {
            button.target = self
            button.bezelStyle = .push
        }
        settings.action = #selector(openSettings(_:))
        rebuild.action = #selector(rebuildIndex(_:))
        editIgnore.action = #selector(editIgnoreList(_:))
        openLoginItems.action = #selector(openLoginItems(_:))
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
        // Focus starts on the folder list, where Space does nothing; on Rebuild Index it started a
        // 10 to 20 s rebuild.
        panel.initialFirstResponder = folderList
        let order: [NSView] = [
            settings, rebuild, everything, cloud, folderList, folderButtons, editIgnore, windowKeys, finderKeys,
            openLoginItems, quit,
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

    private func folderBox() -> NSView {
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("folder"))
        column.resizingMask = .autoresizingMask
        folderList.addTableColumn(column)
        folderList.headerView = nil
        folderList.style = .plain
        folderList.allowsMultipleSelection = true
        folderList.dataSource = self
        folderList.delegate = self
        folderList.onDelete = { [weak self] in self?.removeFolders() }
        folderList.setAccessibilityLabel("Folders searched inside")
        let scroll = NSScrollView()
        scroll.documentView = folderList
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .bezelBorder
        scroll.translatesAutoresizingMaskIntoConstraints = false
        noFolders.textColor = .secondaryLabelColor
        noFolders.translatesAutoresizingMaskIntoConstraints = false
        showEmpty()
        // A scroll view places its own subviews, so the note sits over it in a plain view instead.
        let list = NSView()
        list.addSubview(scroll)
        list.addSubview(noFolders)
        folderButtons.segmentStyle = .smallSquare
        folderButtons.target = self
        folderButtons.action = #selector(changeFolders(_:))
        folderButtons.setToolTip("Add Folder", forSegment: 0)
        folderButtons.setToolTip("Remove Folder", forSegment: 1)
        folderButtons.setEnabled(false, forSegment: 1)
        let hint = NSTextField(wrappingLabelWithString: "Press ⌘T in the search window to find words inside the files in these folders.")
        hint.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        hint.textColor = .secondaryLabelColor
        hint.preferredMaxLayoutWidth = 260
        let box = NSStackView(views: [list, folderButtons, hint])
        box.orientation = .vertical
        box.alignment = .leading
        box.spacing = 0
        box.setCustomSpacing(6, after: folderButtons)
        NSLayoutConstraint.activate([
            list.widthAnchor.constraint(equalToConstant: 260),
            list.heightAnchor.constraint(equalToConstant: 88),
            scroll.topAnchor.constraint(equalTo: list.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: list.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: list.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: list.trailingAnchor),
            noFolders.centerXAnchor.constraint(equalTo: list.centerXAnchor),
            noFolders.centerYAnchor.constraint(equalTo: list.centerYAnchor),
        ])
        return box
    }

    /// Stripes under "No folders yet" would read as rows.
    private func showEmpty() {
        noFolders.isHidden = !folders.paths.isEmpty
        folderList.usesAlternatingRowBackgroundColors = !folders.paths.isEmpty
    }

    @objc func changeFolders(_ sender: Any?) {
        folderButtons.selectedSegment == 0 ? addFolders() : removeFolders()
    }

    func addFolders() {
        choosing = true
        stage.chooseFolders(on: panel) { [weak self] urls in
            guard let self else { return }
            choosing = false
            folders.add(urls.map(\.path))
            reloadFolders()
            if shown { stage.makeKey(panel) }
        }
    }

    func removeFolders() {
        folders.remove(at: folderList.selectedRowIndexes)
        reloadFolders()
        folderList.deselectAll(nil)
        folderButtons.setEnabled(false, forSegment: 1)
    }

    private func reloadFolders() {
        folderList.reloadData()
        showEmpty()
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        folders.paths.count
    }

    func tableView(_ tableView: NSTableView, viewFor column: NSTableColumn?, row: Int) -> NSView? {
        let label = NSTextField(labelWithString: (folders.paths[row] as NSString).abbreviatingWithTildeInPath)
        label.lineBreakMode = .byTruncatingMiddle
        label.toolTip = folders.paths[row]
        return label
    }

    func tableViewSelectionDidChange(_ note: Notification) {
        folderButtons.setEnabled(!folderList.selectedRowIndexes.isEmpty, forSegment: 1)
    }

    func show() {
        shown = true
        refresh()
        // Built before any words were set, so the first show fits it to them; later, only the
        // Open System Settings row changes its height.
        if !fitted {
            fitted = true
            fitKeepingTop()
        }
        stage.bringForward(panel)
        stage.makeKey(panel)
        panel.makeFirstResponder(folderList)
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
        stage.handBack()
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

    /// Adding Argus there stays by hand (M2 decision 8); this only saves finding the page.
    @objc func openLoginItems(_ sender: Any?) {
        hide()
        stage.open(Self.loginItems)
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
        guard !choosing, !sheetUp else { return }
        hide()
    }
}
