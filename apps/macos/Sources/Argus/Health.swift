import AppKit

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
            return health.busy && health.entries == 0 ? "Under way" : "Not since launch; the saved list opened instead"
        }
        return "\(ago(at, now: now)), took \(duration(ms: health.scanMs))"
    }

    func change(_ health: Health, now: Date) -> String {
        health.changedAt.map { ago($0, now: now) } ?? "None since launch"
    }

    func state(_ health: Health) -> String {
        if health.stopped { return "Stopped: quit and reopen Argus" }
        if health.busy { return "Updating…" }
        return health.behind ? "Missing a change: rebuild the index" : "Up to date"
    }

    func access(_ fullDisk: Bool) -> String {
        fullDisk ? "On" : "Off: Desktop, Documents, Downloads and cloud drives are left out"
    }

    /// A whole home folder held 2,478,045 entries on the Mac this was measured on.
    func everything(_ fullDisk: Bool) -> String {
        let count = 2_500_000.formatted(.number.notation(.compactName).locale(locale))
        let what = "Includes Library and clutter, about \(count) items."
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

    override func cancelOperation(_ sender: Any?) {
        (delegate as? HealthWindow)?.hide()
    }
}

/// ⌘,'s window: how many entries the list holds, when it was last read in full and last
/// changed, whether Argus may read every folder, a button to build the list again, the
/// switch to list everything, and the folders whose files' text a search looks inside.
@MainActor
final class HealthWindow: NSObject, NSWindowDelegate, NSTableViewDataSource, NSTableViewDelegate {
    let panel = HealthPanel(
        contentRect: NSRect(x: 0, y: 0, width: 460, height: 200),
        styleMask: [.titled, .closable, .nonactivatingPanel], backing: .buffered, defer: true)
    let entries = NSTextField(labelWithString: "")
    let scanned = NSTextField(labelWithString: "")
    let changed = NSTextField(labelWithString: "")
    let access = NSTextField(wrappingLabelWithString: "")
    let state = NSTextField(labelWithString: "")
    let settings = KeyButton(title: "Open Settings", target: nil, action: nil)
    let rebuild = KeyButton(title: "Rebuild Index", target: nil, action: nil)
    let everything = NSButton(checkboxWithTitle: "Index everything", target: nil, action: nil)
    /// Spins while a whole new list is built, as after flipping the switch: 10 to 20 s.
    let progress = NSProgressIndicator()
    let everythingHint = NSTextField(wrappingLabelWithString: "")
    let grid = NSGridView()
    let folderList = NSTableView()
    let noFolders = NSTextField(labelWithString: "No folders yet")
    let folderButtons = NSSegmentedControl(
        images: [NSImage(named: NSImage.addTemplateName)!, NSImage(named: NSImage.removeTemplateName)!],
        trackingMode: .momentary, target: nil, action: nil)
    let folders: TextFolders
    /// The open panel takes the keys meanwhile, which must not close this window.
    private(set) var choosing = false
    private let read: () -> Health
    private let hasAccess: () -> Bool
    private let onRebuild: () -> Void
    private let isEverything: () -> Bool
    private let onEverything: (Bool) -> Void
    private let stage: any Stage
    private let format: HealthFormat
    private let tick: TimeInterval
    private let dates = DetailsFormat()
    private var ticking: Timer?
    private(set) var shown = false
    private(set) var health = Health()

    init(
        read: @escaping () -> Health, hasAccess: @escaping () -> Bool, rebuild: @escaping () -> Void,
        stage: any Stage, folders: TextFolders = TextFolders(), everything: @escaping () -> Bool = { false },
        indexEverything: @escaping (Bool) -> Void = { _ in }, format: HealthFormat = HealthFormat(),
        tick: TimeInterval = 1
    ) {
        self.folders = folders
        self.read = read
        self.hasAccess = hasAccess
        onRebuild = rebuild
        isEverything = everything
        onEverything = indexEverything
        self.stage = stage
        self.format = format
        self.tick = tick
        super.init()
        build()
    }

    private func build() {
        let label = { (text: String) -> NSView in NSTextField(labelWithString: text) }
        let rows: [[NSView]] = [
            [label("Entries:"), entries],
            [label("Last full scan:"), scanned],
            [label("Last change applied:"), changed],
            [label("Status:"), state],
            [label("Full Disk Access:"), access],
            [NSGridCell.emptyContentView, settings],
            [NSGridCell.emptyContentView, rebuild],
            [NSGridCell.emptyContentView, everythingBox()],
            [label("Search inside files in:"), folderBox()],
        ]
        for row in rows { grid.addRow(with: row) }
        grid.row(at: 7).topPadding = 12
        grid.row(at: 8).topPadding = 12
        panel.title = "Index Health"
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.hidesOnDeactivate = false
        panel.isReleasedWhenClosed = false
        panel.delegate = self
        access.preferredMaxLayoutWidth = 260
        for button in [settings, rebuild] {
            button.target = self
            button.bezelStyle = .push
        }
        settings.action = #selector(openSettings(_:))
        rebuild.action = #selector(rebuildIndex(_:))
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 8
        grid.columnSpacing = 8
        grid.row(at: 6).topPadding = 12
        grid.translatesAutoresizingMaskIntoConstraints = false
        let content = NSView()
        content.addSubview(grid)
        NSLayoutConstraint.activate([
            grid.topAnchor.constraint(equalTo: content.topAnchor, constant: 20),
            grid.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -20),
            grid.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 20),
            grid.trailingAnchor.constraint(lessThanOrEqualTo: content.trailingAnchor, constant: -20),
            content.widthAnchor.constraint(equalToConstant: 460),
        ])
        panel.contentView = content
        panel.initialFirstResponder = rebuild
        rebuild.nextKeyView = settings
        settings.nextKeyView = everything
        everything.nextKeyView = folderList
        folderList.nextKeyView = folderButtons
        folderButtons.nextKeyView = rebuild
        panel.setContentSize(content.fittingSize)
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
        let line = NSStackView(views: [everything, progress])
        line.spacing = 6
        let box = NSStackView(views: [line, everythingHint])
        box.orientation = .vertical
        box.alignment = .leading
        box.spacing = 2
        return box
    }

    @objc func switchEverything(_ sender: Any?) {
        onEverything(everything.state == .on)
        refresh()
    }

    private func folderBox() -> NSView {
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("folder"))
        column.resizingMask = .autoresizingMask
        folderList.addTableColumn(column)
        folderList.headerView = nil
        folderList.style = .plain
        folderList.usesAlternatingRowBackgroundColors = true
        folderList.allowsMultipleSelection = true
        folderList.dataSource = self
        folderList.delegate = self
        folderList.setAccessibilityLabel("Folders searched inside")
        let scroll = NSScrollView()
        scroll.documentView = folderList
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .bezelBorder
        noFolders.textColor = .secondaryLabelColor
        noFolders.translatesAutoresizingMaskIntoConstraints = false
        noFolders.isHidden = !folders.paths.isEmpty
        scroll.addSubview(noFolders)
        folderButtons.segmentStyle = .smallSquare
        folderButtons.target = self
        folderButtons.action = #selector(changeFolders(_:))
        folderButtons.setToolTip("Add Folder", forSegment: 0)
        folderButtons.setToolTip("Remove Folder", forSegment: 1)
        folderButtons.setEnabled(false, forSegment: 1)
        let hint = NSTextField(wrappingLabelWithString: "Typing also finds words inside the files in these folders.")
        hint.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        hint.textColor = .secondaryLabelColor
        hint.preferredMaxLayoutWidth = 260
        let box = NSStackView(views: [scroll, folderButtons, hint])
        box.orientation = .vertical
        box.alignment = .leading
        box.spacing = 0
        box.setCustomSpacing(6, after: folderButtons)
        NSLayoutConstraint.activate([
            scroll.widthAnchor.constraint(equalToConstant: 260),
            scroll.heightAnchor.constraint(equalToConstant: 88),
            noFolders.centerXAnchor.constraint(equalTo: scroll.centerXAnchor),
            noFolders.centerYAnchor.constraint(equalTo: scroll.centerYAnchor),
        ])
        return box
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
        noFolders.isHidden = !folders.paths.isEmpty
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
        stage.bringForward(panel)
        stage.makeKey(panel)
        panel.makeFirstResponder(rebuild)
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

    /// The Open Settings row comes and goes; the title bar stays where it was.
    private func fitKeepingTop() {
        guard let content = panel.contentView else { return }
        var frame = panel.frameRect(forContentRect: NSRect(origin: .zero, size: content.fittingSize))
        frame.origin = NSPoint(x: panel.frame.minX, y: panel.frame.maxY - frame.height)
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

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        hide()
        return false
    }

    /// Like the search window, it goes once you click in another app.
    func windowDidResignKey(_ note: Notification) {
        guard !choosing else { return }
        hide()
    }
}
