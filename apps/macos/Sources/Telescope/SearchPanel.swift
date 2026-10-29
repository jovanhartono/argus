import AppKit
import OSLog
import QuickLookThumbnailing
import QuickLookUI
import UniformTypeIdentifiers

enum Metric {
    static let width: CGFloat = 680
    static let field: CGFloat = 56
    static let row: CGFloat = 48
    static let header: CGFloat = 28
    static let visibleRows = 8
    static let inset: CGFloat = 8
    static let message: CGFloat = 48
    static let fade: TimeInterval = 0.12
    static let resize: TimeInterval = 0.15
    static let pane: CGFloat = 260
    static let thumb: CGFloat = 128
    static let details: CGFloat = 286
    static let chip: CGFloat = 240
    static let chipHeight: CGFloat = 24
}

/// Read with `log show --last 5m --predicate 'subsystem == "io.github.jovanhartono.telescope"'`.
let timing = Logger(subsystem: "io.github.jovanhartono.telescope", category: "timing")
let failures = Logger(subsystem: "io.github.jovanhartono.telescope", category: "failure")

final class Panel: NSPanel {
    override var canBecomeKey: Bool { true }

    /// Esc while Open Settings has the focus; the box handles its own Esc. A preview closes first.
    override func cancelOperation(_ sender: Any?) {
        guard let search = delegate as? SearchPanel else { return }
        search.previewing ? search.closePreview() : search.hide()
    }

    /// ⌘W does what Esc does; with no close button, AppKit would only beep.
    override func performClose(_ sender: Any?) {
        cancelOperation(sender)
    }

    override func validateMenuItem(_ item: NSMenuItem) -> Bool {
        item.action == #selector(performClose(_:)) || super.validateMenuItem(item)
    }

    override func acceptsPreviewPanelControl(_ panel: QLPreviewPanel!) -> Bool {
        true
    }

    override func beginPreviewPanelControl(_ panel: QLPreviewPanel!) {
        MainActor.assumeIsolated {
            // Quick Look sits at the normal level, under this floating window, by default, and
            // can't join a full-screen app's space.
            panel.level = level
            panel.collectionBehavior = collectionBehavior
            panel.dataSource = delegate as? SearchPanel
            panel.delegate = delegate
        }
    }

    override func endPreviewPanelControl(_ panel: QLPreviewPanel!) {
        MainActor.assumeIsolated {
            panel.dataSource = nil
            panel.delegate = nil
            (delegate as? SearchPanel)?.settleKeys()
        }
    }
}

/// Lays out from the top, so the box stays put while the window grows or shrinks below it.
final class TopDown: NSView {
    override var isFlipped: Bool { true }
}

/// Tab reaches it even with Keyboard Navigation off in System Settings: Telescope has no Dock
/// icon or menu bar, so without this a keyboard user could never grant access.
final class KeyButton: NSButton {
    override var acceptsFirstResponder: Bool { true }
}

@MainActor
final class SearchPanel: NSObject, NSWindowDelegate, NSTextFieldDelegate, NSTableViewDataSource,
    NSTableViewDelegate, @preconcurrency QLPreviewPanelDataSource, @preconcurrency QLPreviewPanelDelegate
{
    let panel = Panel(
        contentRect: NSRect(x: 0, y: 0, width: Metric.width, height: Metric.field),
        styleMask: [.titled, .fullSizeContentView, .nonactivatingPanel],
        backing: .buffered, defer: false)
    private let glass = NSImageView(frame: NSRect(x: 18, y: 16, width: 24, height: 24))
    let field = NSTextField(frame: NSRect(x: 50, y: 13, width: Metric.width - 68, height: 30))
    /// The folders Tab went into, outermost first, shown before the box.
    let chip = NSBox()
    let chipText = NSTextField(labelWithString: "")
    private(set) var scopes: [Row] = []
    private let line = NSBox(frame: NSRect(x: 0, y: Metric.field, width: Metric.width, height: 1))
    private let scroll = NSScrollView()
    let table = NSTableView()
    private let message = NSTextField(labelWithString: "")
    private let bar = TopDown()
    private let barLine = NSBox(frame: NSRect(x: 0, y: 0, width: Metric.width, height: 1))
    let barText = NSTextField(labelWithString: "")
    let settings = KeyButton(title: "Open Settings", target: nil, action: nil)
    let pane = DetailsPane()
    /// ⌘I's switch, kept while the window hides; the pane itself shows only beside a picked row.
    private(set) var detailsOn = false
    /// How far the pane pushed the window left to fit on the screen; closing it moves the box back.
    private var nudge: CGFloat = 0

    private var hatch: Hatch?
    private let hasAccess: () -> Bool
    /// Full Disk Access was off when last checked, so the list leaves the guarded folders out.
    private(set) var lacksAccess = false
    private let stage: any Stage
    private(set) var notice: Notice?
    private let finder: @Sendable () -> FinderFolder
    /// The first time, asking waits for your answer to macOS's question, so it never holds a
    /// thread Swift's tasks share, and presses meanwhile queue up behind it.
    private let finderQueue = DispatchQueue(label: "io.github.jovanhartono.telescope.finder", qos: .userInitiated)
    /// Counts presses of the Finder hotkey and shows, so only the answer to the newest press
    /// opens the window, and only if it hasn't been opened since.
    private var finderAsks = 0
    private(set) var health: HealthWindow?
    private var indexingSince: TimeInterval = 0
    private(set) var indexed = false
    /// The first list was the one saved at the last launch.
    private(set) var reopened = false
    private var problem: String?
    /// The file you picked was moved or deleted after the list was built.
    private(set) var missing: String?
    private var newest: UInt64 = 0
    private var sentAt: [UInt64: TimeInterval] = [:]
    private(set) var rows: [Row] = []
    let textFolders: TextFolders
    let everything: IndexEverything
    private(set) var hits: [TextHit] = []
    private(set) var textTicket: UInt64 = 0
    private var textAsked: TextAsk?
    /// The search for those lines ran to the end while the window showed, and no file changed
    /// since it started. Editing a file's text brings no new list, so each show reads again.
    private var textWhole = false
    /// A search run again for the same words leaves the old lines up until its own come, so the
    /// window doesn't shrink and grow back.
    private var replacing = false
    private var filesChangedAt: TimeInterval = 0
    /// When macOS told of each drive plugged in, until a list holding it is in use.
    var drivesTold: [String: TimeInterval] = [:]
    /// When the key behind the text search under way went down; nil for a search run again.
    private var textTypedAt: TimeInterval?
    private var textStartedAt: TimeInterval = 0
    private var textDue = false
    private var shownHeader: Int?
    /// The list changed while the window was hidden, so the results on screen may be old.
    private(set) var stale = false
    /// A search run again on a new list, which keeps the selected file when it answers.
    private var rerun: UInt64 = 0
    /// The ticket the rows on screen answer; 0 before any answer, so an empty list isn't "No Results".
    private(set) var answered: UInt64 = 0
    private var pending: (() -> Void)?
    /// The file the open preview shows, so a new list with the same file picked doesn't reload it.
    private(set) var inPreview: URL?
    private var closing = false
    private(set) var shown = false
    /// By kind of file, or by path for an app, which has its own.
    private var icons: [String: NSImage] = [:]
    private(set) var readingIcons: Set<String> = []
    private let appFolders: [String]

    /// With `saved`, the list kept in that file at the last launch shows at once. With `opens`,
    /// what you open ranks higher, kept in that file. The apps in `apps` are found by name, and
    /// words inside the files in `textFolders`. With `everything` on, clutter is listed too.
    init(
        root: String = NSHomeDirectory(), saved: String? = nil, opens: String? = nil, apps: [String] = [],
        textFolders: TextFolders = TextFolders(), everything: IndexEverything = IndexEverything(),
        hasAccess: @escaping () -> Bool = { Hatch.hasFullDiskAccess() }, stage: any Stage = Screen(),
        finder: @escaping @Sendable () -> FinderFolder = FinderFolder.ask
    ) {
        self.textFolders = textFolders
        self.everything = everything
        self.hasAccess = hasAccess
        self.stage = stage
        self.finder = finder
        appFolders = apps
        super.init()
        build()
        let onEvent: (Hatch.Event) -> Void = { [weak self] event in self?.heard(event) }
        // TELESCOPE_STAND_IN=1000000 searches made-up names instead of your files, and 0 measures
        // the window's own memory without any list.
        indexingSince = ProcessInfo.processInfo.systemUptime
        if let count = ProcessInfo.processInfo.environment["TELESCOPE_STAND_IN"].flatMap(UInt64.init) {
            hatch = Hatch(standIn: count, onEvent: onEvent)
        } else {
            lacksAccess = !hasAccess()
            post(lacksAccess ? .needsAccess : nil)
            // Without Full Disk Access, reading all of ~/Library and the media folders stops on
            // a macOS pop-up for each of several libraries, so the switch waits for access.
            hatch = Hatch(
                root: root, fullDisk: !lacksAccess, everything: everything.isOn && !lacksAccess, saved: saved,
                onEvent: onEvent)
        }
        if hatch == nil { problem = "Search couldn't start. Rebuild Telescope." }
        if let opens { hatch?.rankOpens(keptIn: opens) }
        fit()
    }

    func toggle() {
        shown ? hide() : show()
    }

    /// The very first show costs 30-50 ms, so it happens once at launch, out of sight.
    func warmUp() {
        panel.alphaValue = 0
        place()
        stage.bringForward(panel)
        panel.display()
        stage.putAway(panel)
    }

    /// `needsFinder` shows the Finder line, which only the Finder hotkey's window has.
    func show(needsFinder: Bool = false) {
        let pressed = pressedAt()
        finderAsks += 1
        recheckAccess()
        if needsFinder != (notice == .needsFinder) {
            post(needsFinder ? .needsFinder : lacksAccess ? .needsAccess : nil)
            fit()
        }
        // Read again on each show, about 0.3 ms off the main thread, so a new app is found.
        if !appFolders.isEmpty { hatch?.readApps(in: appFolders) }
        if stale { runAgain() }
        // A file saved while the window was hidden shows its new size and date.
        showDetails()
        shown = true
        place()
        if answered == newest { startText(textAsk(), typedAt: nil) }
        panel.alphaValue = reduceMotion ? 1 : 0
        stage.bringForward(panel)
        stage.makeKey(panel)
        // Your last search comes back selected, so typing replaces it.
        if let editor = field.currentEditor() {
            editor.selectAll(nil)
        } else {
            panel.makeFirstResponder(field)
            field.selectText(nil)
        }
        panel.displayIfNeeded()
        timing.notice("window drawn \(ms(since: pressed), format: .fixed(precision: 2)) ms after the hotkey")
        fade(to: 1)
    }

    func hide() {
        guard shown else { return }
        shown = false
        pending = nil
        stopText()
        textWhole = false
        closePreview()
        // A word left half-typed when you click away is kept as typed, so searching isn't
        // blocked once you're back.
        if composing, let editor = field.currentEditor() as? NSTextView {
            editor.inputContext?.discardMarkedText()
            editor.unmarkText()
            search()
        }
        fade(to: 0) { [weak self] in
            // Shown again while fading out: keep it.
            guard let self, !shown else { return }
            stage.putAway(panel)
            if notice == .needsFinder {
                post(lacksAccess ? .needsAccess : nil)
                fit()
            }
        }
    }

    /// The Finder hotkey: the window opens searching inside the folder the front Finder window
    /// shows, or as you left it when no Finder window shows a folder the list holds.
    func toggleInFinderFolder() {
        if shown { return hide() }
        finderAsks += 1
        let ask = finderAsks
        let pressed = pressedAt()
        let finder = finder
        finderQueue.async { [weak self] in
            let answer = finder()
            Task { @MainActor in self?.heardFinder(answer, ask: ask, pressed: pressed) }
        }
    }

    private func heardFinder(_ answer: FinderFolder, ask: Int, pressed: TimeInterval) {
        guard ask == finderAsks, !shown else { return }
        timing.notice("Finder's folder read \(ms(since: pressed), format: .fixed(precision: 2)) ms after the hotkey")
        switch answer {
        // Without Full Disk Access, Finder on Downloads would otherwise search an empty list.
        case .folder(let path) where hatch?.holds(folder: path) == true:
            scopes = [Row(folderAt: path)]
            field.stringValue = ""
            showScopes()
            search()
        case .folder, .none:
            break
        case .denied:
            return show(needsFinder: true)
        case .failed(let code):
            failures.error("Finder didn't tell its folder: \(code)")
        }
        show()
    }

    /// ⌘,: the search window steps aside for the health view.
    @objc func showHealth(_ sender: Any?) {
        hide()
        if health == nil {
            health = HealthWindow(
                read: { [weak self] in self?.hatch?.health() ?? Health(stopped: true) },
                hasAccess: { [weak self] in
                    guard let self else { return false }
                    recheckAccess()
                    return !lacksAccess
                },
                rebuild: { [weak self] in self?.hatch?.rebuild() }, stage: stage, folders: textFolders,
                everything: { [weak self] in self?.everything.isOn ?? false },
                indexEverything: { [weak self] on in
                    guard let self else { return }
                    everything.isOn = on
                    if !lacksAccess { hatch?.indexEverything(on) }
                })
        }
        health?.show()
    }

    func driveMounted(_ path: String) {
        let told = ProcessInfo.processInfo.systemUptime
        guard hatch?.volumeAdded(path) == true, indexed else { return }
        drivesTold[path] = told
    }

    func driveUnmounting(_ path: String) {
        let told = ProcessInfo.processInfo.systemUptime
        guard hatch?.volumeRemoved(path) == true else { return }
        drivesTold[path] = nil
        timing.notice("let go of a drive \(ms(since: told), format: .fixed(precision: 2)) ms after macOS told")
    }

    /// A list built for a change at home can land first, so only one holding the drive counts.
    func timeDrives(watching: [String]) {
        for (path, told) in drivesTold where watching.contains(path) {
            timing.notice("drive findable \(ms(since: told), format: .fixed(precision: 0)) ms after macOS told")
            drivesTold[path] = nil
        }
    }

    private func build() {
        panel.titlebarAppearsTransparent = true
        panel.titleVisibility = .hidden
        for button in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
            panel.standardWindowButton(button)?.isHidden = true
        }
        panel.isMovableByWindowBackground = true
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.backgroundColor = .windowBackgroundColor
        panel.hidesOnDeactivate = false
        // Panels zoom in and out by default; the fade is the only motion wanted.
        panel.animationBehavior = .none
        panel.delegate = self

        glass.image = NSImage(systemSymbolName: "magnifyingglass", accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 20, weight: .regular))
        glass.contentTintColor = .secondaryLabelColor

        field.font = .systemFont(ofSize: 22)
        field.placeholderString = "Search"
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.cell?.usesSingleLineMode = true
        field.cell?.lineBreakMode = .byTruncatingTail
        field.delegate = self

        chip.boxType = .custom
        chip.titlePosition = .noTitle
        chip.borderWidth = 0
        chip.cornerRadius = 6
        chip.fillColor = .quaternaryLabelColor
        chip.contentViewMargins = .zero
        chip.isHidden = true
        chipText.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .medium)
        chipText.lineBreakMode = .byTruncatingHead
        chip.contentView?.addSubview(chipText)

        line.boxType = .separator
        line.autoresizingMask = [.width]

        let column = NSTableColumn(identifier: ResultCell.id)
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        table.headerView = nil
        table.rowHeight = Metric.row
        table.style = .inset
        table.intercellSpacing = .zero
        table.backgroundColor = .clear
        table.refusesFirstResponder = true
        table.dataSource = self
        table.delegate = self
        scroll.documentView = table
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true

        message.font = .systemFont(ofSize: NSFont.systemFontSize)
        message.textColor = .secondaryLabelColor
        message.alignment = .center
        message.lineBreakMode = .byTruncatingMiddle
        message.frame = NSRect(
            x: 16, y: Metric.field + 1 + (Metric.message - 17) / 2, width: Metric.width - 32,
            height: 17)

        barLine.boxType = .separator
        barLine.autoresizingMask = [.width]
        barText.font = message.font
        barText.textColor = .secondaryLabelColor
        barText.lineBreakMode = .byTruncatingTail
        settings.target = self
        settings.action = #selector(openSettings(_:))
        settings.sizeToFit()
        settings.setFrameOrigin(
            NSPoint(
                x: Metric.width - 16 - settings.frame.width,
                y: 1 + (Metric.message - settings.frame.height) / 2))
        settings.autoresizingMask = [.minXMargin]
        for view in [barLine, barText, settings] { bar.addSubview(view) }
        bar.frame = NSRect(x: 0, y: Metric.field, width: Metric.width, height: 1 + Metric.message)
        bar.autoresizingMask = [.width]
        bar.isHidden = true
        pane.isHidden = true
        field.nextKeyView = settings
        settings.nextKeyView = field

        // Starting at the window's size, the views that stretch with it start at the right width.
        let content = TopDown(frame: NSRect(x: 0, y: 0, width: Metric.width, height: Metric.field))
        for view in [glass, chip, field, line, scroll, message, bar, pane] { content.addSubview(view) }
        panel.contentView = content
    }

    /// Checked on each show while access is missing, so the line goes once access is switched
    /// on in System Settings and Later is clicked, and the guarded folders come in.
    func recheckAccess() {
        guard lacksAccess, hasAccess() else { return }
        lacksAccess = false
        if notice == .needsAccess { post(nil) }
        hatch?.grantFullDisk(everything: everything.isOn)
        fit()
    }

    private func post(_ fresh: Notice?) {
        notice = fresh
        bar.isHidden = fresh == nil
        settings.isHidden = fresh == nil
        barText.stringValue = fresh?.text ?? ""
        let textWidth = Metric.width - 32 - (settings.isHidden ? 0 : settings.frame.width + 12)
        barText.frame = NSRect(x: 16, y: 1 + (Metric.message - 17) / 2, width: textWidth, height: 17)
        line.frame.origin.y = head
        message.frame.origin.y = head + 1 + (Metric.message - 17) / 2
    }

    /// Where the results start. The access line sits right under the box and never moves:
    /// kept at the bottom, it moved with every resize and doubled the keystroke-to-results time.
    private var head: CGFloat {
        Metric.field + (notice == nil ? 0 : 1 + Metric.message)
    }

    @objc private func openSettings(_ sender: Any?) {
        guard let page = notice?.settings else { return }
        hide()
        stage.open(page)
    }

    /// A fifth of the way down the screen that has the mouse.
    private func place() {
        let mouse = NSEvent.mouseLocation
        let screen = NSScreen.screens.first { NSMouseInRect(mouse, $0.frame, false) } ?? .main
        guard let visible = screen?.visibleFrame else { return }
        let height = panel.frame.height
        let top = visible.maxY - visible.height * 0.2
        let width = width(detailing)
        let centred = visible.midX - Metric.width / 2
        let x = min(centred, visible.maxX - width)
        nudge = centred - x
        let frame = NSRect(x: x, y: top - height, width: width, height: height)
        // Going through the animator stops a resize still running from before the window hid.
        NSAnimationContext.runAnimationGroup { context in
            context.duration = 0
            panel.animator().setFrame(frame, display: false)
        }
    }

    private var reduceMotion: Bool {
        NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
    }

    /// Japanese and Chinese input hold a half-typed word as "marked text" until it is picked.
    private var composing: Bool {
        (panel.fieldEditor(false, for: field) as? NSTextView)?.hasMarkedText() ?? false
    }

    var status: String? {
        if let problem { return problem }
        if composing { return nil }
        if !indexed { return "Getting ready…" }
        if let missing { return missing }
        if answered != 0 && rows.isEmpty && hits.isEmpty {
            return textTicket == 0 && !textDue ? "No Results" : "Searching inside files…"
        }
        return nil
    }

    private func bodyHeight(_ status: String?, _ detailing: Bool) -> CGFloat {
        if field.stringValue.isEmpty { return 0 }
        if status != nil { return Metric.message }
        if listed == 0 { return 0 }
        var list = CGFloat(min(listed, Metric.visibleRows)) * Metric.row + 2 * Metric.inset
        if let header, header < Metric.visibleRows { list -= Metric.row - Metric.header }
        return detailing ? max(list, Metric.details) : list
    }

    var listed: Int {
        rows.count + (hits.isEmpty ? 0 : 1 + hits.count)
    }

    var header: Int? {
        hits.isEmpty ? nil : rows.count
    }

    private func hit(at index: Int) -> TextHit? {
        let at = index - rows.count - 1
        return hits.indices.contains(at) ? hits[at] : nil
    }

    private var top: Int? {
        rows.isEmpty ? (hits.isEmpty ? nil : rows.count + 1) : 0
    }

    var detailing: Bool {
        detailsOn && selected != nil && status == nil && !field.stringValue.isEmpty
    }

    private func width(_ detailing: Bool) -> CGFloat {
        Metric.width + (detailing ? 1 + Metric.pane : 0)
    }

    /// The box alone while it is empty; it grows downward to fit up to 8 rows, and to the right
    /// while the details pane is on.
    private func fit() {
        let status = self.status
        let detailing = self.detailing
        let body = bodyHeight(status, detailing)
        let width = width(detailing)
        message.stringValue = status ?? ""
        line.isHidden = body == 0
        scroll.isHidden = body == 0 || status != nil
        message.isHidden = body == 0 || status == nil
        scroll.frame = NSRect(x: 0, y: head + 1, width: Metric.width, height: body)
        pane.isHidden = !detailing
        if detailing { pane.frame = NSRect(x: Metric.width, y: head + 1, width: 1 + Metric.pane, height: body) }

        let height = head + (body > 0 ? 1 + body : 0)
        var frame = panel.frame
        guard frame.height != height || frame.width != width else { return }
        frame.origin.y += frame.height - height
        frame.size.height = height
        frame.size.width = width
        frame.origin.x += nudge
        nudge = 0
        if let visible = panel.screen?.visibleFrame, frame.maxX > visible.maxX {
            let x = max(visible.minX, visible.maxX - width)
            nudge = frame.origin.x - x
            frame.origin.x = x
        }
        guard shown, !reduceMotion else {
            panel.setFrame(frame, display: true)
            return
        }
        NSAnimationContext.runAnimationGroup { context in
            context.duration = Metric.resize
            panel.animator().setFrame(frame, display: true)
        }
    }

    private func fade(to alpha: CGFloat, then done: (@MainActor () -> Void)? = nil) {
        NSAnimationContext.runAnimationGroup { context in
            context.duration = reduceMotion ? 0 : Metric.fade
            panel.animator().alphaValue = alpha
        } completionHandler: {
            MainActor.assumeIsolated { done?() }
        }
    }

    private func heard(_ event: Hatch.Event) {
        switch event {
        case .indexed(let count), .opened(let count):
            let took = ms(since: indexingSince)
            if case .opened = event {
                reopened = true
                timing.notice("opened \(count) saved entries in \(took, format: .fixed(precision: 0)) ms")
            } else {
                timing.notice("indexed \(count) entries in \(took, format: .fixed(precision: 0)) ms")
            }
            indexed = true
            hatch?.watch()
            search()
            health?.refresh()
        case .rebuilt(let count, let took):
            timing.notice("rebuilt \(count) entries in \(took) ms after a change")
            hatch?.watch()
            if !drivesTold.isEmpty, let hatch { timeDrives(watching: hatch.watching) }
            filesChangedAt = ProcessInfo.processInfo.systemUptime
            textWhole = false
            if shown { runAgain() } else { stale = true }
            health?.refresh()
        case .reranked:
            if shown { runAgain() } else { stale = true }
        case .ready(let ticket) where ticket == newest:
            take(ticket)
        case .ready:
            break
        case .lines(let ticket) where ticket == textTicket:
            takeText(ticket)
        case .lines:
            break
        case .failed(let code) where code != 0:
            let reason = String(cString: strerror(code))
            failures.error("home folder scan failed: \(reason, privacy: .public)")
            problem = "Telescope can't read your home folder: \(reason)."
            fit()
        case .failed:
            failures.error("the engine stopped")
            problem = "Search stopped working. Quit and reopen Telescope."
            fit()
        }
    }

    private func search(typedAt typed: TimeInterval = ProcessInfo.processInfo.systemUptime) {
        missing = nil
        pending = nil
        // Each key stops the text search at once; the next starts once the names are drawn.
        stopText()
        if field.stringValue.isEmpty {
            // A reply to what was typed before the box was cleared must not fill it again.
            newest = 0
            sentAt = [:]
            rows = []
            dropLines()
            answered = 0
            refill()
            follow()
        }
        guard let hatch, indexed, !composing, !field.stringValue.isEmpty else { return fit() }
        newest = hatch.submit(field.stringValue, in: scopes.last?.path)
        sentAt[newest] = typed
        fit()
    }

    /// The same search on a new list. No keystroke started it, so it writes no timing line.
    private func runAgain() {
        stale = false
        guard let hatch, indexed, !composing, !field.stringValue.isEmpty else { return }
        newest = hatch.submit(field.stringValue, in: scopes.last?.path)
        rerun = newest
    }

    /// Pressed a moment after typing, Tab waits for the results for what you typed, as Return
    /// does; if they start with a file, it still reaches Open Settings.
    private func tabIntoFolder() -> Bool {
        guard indexed, !composing, !field.stringValue.isEmpty else { return false }
        if newest == answered && selected?.isFolder != true { return false }
        onceAnswered { [weak self] in
            guard let self else { return }
            if let row = selected, row.isFolder {
                enter(row)
            } else {
                _ = reachSettings()
            }
        }
        return true
    }

    private func reachSettings() -> Bool {
        guard notice != nil else { return false }
        panel.makeFirstResponder(settings)
        return true
    }

    private func enter(_ folder: Row) {
        scopes.append(folder)
        field.stringValue = ""
        showScopes()
        search()
    }

    private func leaveFolder() -> Bool {
        guard !scopes.isEmpty, !composing, field.stringValue.isEmpty else { return false }
        // Holding Backspace to clear the box stops there instead of leaving every folder too.
        if let event = NSApp.currentEvent, event.type == .keyDown, event.isARepeat { return true }
        scopes.removeLast()
        showScopes()
        search()
        return true
    }

    private func showScopes() {
        chip.isHidden = scopes.isEmpty
        chipText.stringValue = scopes.map(\.name).joined(separator: " › ")
        chip.toolTip = scopes.last?.path
        chipText.setAccessibilityLabel(scopes.last.map { "Searching in \($0.name)" })
        field.placeholderString = scopes.last.map { "Search in \($0.name)" } ?? "Search"
        let padding: CGFloat = 8
        // The label's own size is a few points short of what it draws, which cut "pdf" to "…f".
        let size = chipText.cell!.cellSize
        let text = min(size.width, Metric.chip - 2 * padding)
        let height = size.height
        chipText.frame = NSRect(x: padding, y: (Metric.chipHeight - height) / 2, width: text, height: height)
        chip.frame = NSRect(
            x: 50, y: (Metric.field - Metric.chipHeight) / 2, width: text + 2 * padding, height: Metric.chipHeight)
        let x = scopes.isEmpty ? 50 : chip.frame.maxX + 8
        field.frame = NSRect(x: x, y: 13, width: Metric.width - 18 - x, height: 30)
    }

    private func take(_ ticket: UInt64) {
        guard let fresh = hatch?.take(ticket) else { return }
        // A file you picked stays picked when a change moves it up or down the list.
        let keeping = rerun == ticket ? selected?.path : nil
        let keepingLine = rerun == ticket ? table.selectedRow - rows.count - 1 : -1
        rerun = 0
        let ask = textAsk()
        // Lines found for what was typed before go with its names.
        if ask != textAsked { dropLines() }
        rows = fresh
        answered = ticket
        refill()
        if hits.indices.contains(keepingLine) {
            select(rows.count + 1 + keepingLine)
        } else if let top {
            select(rows.firstIndex { $0.path == keeping } ?? top)
        }
        // Picking the same row again tells the table nothing, yet it may hold another file now.
        showDetails()
        follow()
        let typed = sentAt.removeValue(forKey: ticket)
        sentAt = sentAt.filter { $0.key > ticket }
        // Started after the names' first frame, which it would slow by 0.3-0.5 ms; that frame
        // already says "Searching inside files…", not "No Results".
        textDue = shown && needsText(ask)
        fit()
        if let action = pending {
            pending = nil
            action()
        } else if let typed {
            // The rows grow into view over the 0.15 s resize; this times their first frame.
            panel.displayIfNeeded()
            timing.notice("results drawn \(ms(since: typed), format: .fixed(precision: 2)) ms after the keystroke")
        }
        startText(ask, typedAt: typed)
    }

    private func needsText(_ ask: TextAsk?) -> Bool {
        ask != nil && (ask != textAsked || !textWhole && textTicket == 0)
    }

    /// What a text search for the box would look for, or nil when no chosen folder is reached.
    private func textAsk() -> TextAsk? {
        guard !field.stringValue.isEmpty, !composing else { return nil }
        let folders = textFolders.reached(within: scopes.last?.path)
        return folders.isEmpty ? nil : TextAsk(query: field.stringValue, folders: folders)
    }

    /// Only while the window shows. The same words are read again once the last search for them
    /// was stopped by hiding, failed, or saw a file change; never while one runs, so files that
    /// keep changing can't keep restarting it.
    private func startText(_ ask: TextAsk?, typedAt typed: TimeInterval?) {
        textDue = false
        guard shown, let hatch else { return }
        if ask != textAsked {
            let had = !hits.isEmpty
            dropLines()
            if had { refill() }
        }
        guard let ask, needsText(ask) else { return fit() }
        replacing = !hits.isEmpty
        textAsked = ask
        textTypedAt = typed
        textStartedAt = ProcessInfo.processInfo.systemUptime
        textTicket = hatch.searchText(ask.query, in: ask.folders)
        if textTicket == 0 {
            textAsked = nil
            replacing = false
        }
        fit()
    }

    private func dropLines() {
        hits = []
        textAsked = nil
        textWhole = false
        replacing = false
    }

    private func stopText() {
        guard textTicket != 0 else { return }
        hatch?.cancelText()
        textTicket = 0
    }

    private func takeText(_ ticket: UInt64) {
        guard let batch = hatch?.takeText(ticket) else { return }
        let replaced = replacing && (!batch.hits.isEmpty || batch.done)
        let picked = replaced ? hit(at: table.selectedRow) : nil
        if replaced {
            replacing = false
            hits = []
        }
        let first = hits.isEmpty && !batch.hits.isEmpty
        if replaced || !batch.hits.isEmpty {
            hits += batch.hits
            refill()
            if let picked, let at = hits.firstIndex(where: { $0.path == picked.path && $0.line == picked.line }) {
                select(rows.count + 1 + at)
            } else if table.selectedRow < 0, let top {
                select(top)
            }
            if replaced {
                showDetails()
                follow()
            }
        }
        if batch.done {
            textTicket = 0
            textWhole = !batch.failed && filesChangedAt < textStartedAt
            if batch.failed { failures.error("a text search stopped early") }
            let took = ms(since: textStartedAt)
            timing.notice(
                "text search found \(self.hits.count) lines in \(batch.filesSearched) files in \(took, format: .fixed(precision: 1)) ms"
            )
        }
        fit()
        if first, let typed = textTypedAt {
            panel.displayIfNeeded()
            timing.notice("first text line drawn \(ms(since: typed), format: .fixed(precision: 2)) ms after the keystroke")
        }
    }

    /// Return pressed a moment after typing acts on the results for what you typed, not on the
    /// older list still on screen.
    private func onceAnswered(_ action: @escaping () -> Void) {
        if newest != answered { pending = action } else { action() }
    }

    private func select(_ row: Int) {
        table.selectRowIndexes([row], byExtendingSelection: false)
        table.scrollRowToVisible(row)
    }

    /// The first arrow after "can't be found" brings the list back, on the same row.
    private func move(by step: Int) {
        if missing != nil {
            missing = nil
            return fit()
        }
        guard listed > 0 else { return }
        let pressed = pressedAt()
        let before = table.selectedRow
        var next = min(max(before + step, 0), listed - 1)
        if next == header { next = (0..<listed).contains(next + step) ? next + step : before }
        guard next >= 0 else { return }
        select(next)
        guard detailing, table.selectedRow != before else { return }
        panel.displayIfNeeded()
        timing.notice("details drawn \(ms(since: pressed), format: .fixed(precision: 2)) ms after the arrow key")
    }

    /// A line found inside a file stands for its file.
    var selected: Row? {
        rows.indices.contains(table.selectedRow) ? rows[table.selectedRow] : hit(at: table.selectedRow)?.file
    }

    private func found(_ row: Row) -> Bool {
        guard FileManager.default.fileExists(atPath: row.path) else {
            missing = "“\(row.name)” can’t be found."
            closePreview()
            fit()
            return false
        }
        return true
    }

    /// Japanese and Chinese input pick the half-typed word with Return, so nothing opens then.
    private func open(revealing reveal: Bool) {
        guard !composing, let row = selected, found(row) else { return }
        hide()
        if reveal {
            stage.showInFinder(row.url)
        } else {
            stage.open(row.url)
        }
        hatch?.opened(row.path)
    }

    @objc func showInFinder(_ sender: Any?) {
        onceAnswered { [weak self] in self?.open(revealing: true) }
    }

    @objc func copyPath(_ sender: Any?) {
        onceAnswered { [weak self] in
            guard let self, !composing, let row = selected else { return }
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(row.path, forType: .string)
            hide()
        }
    }

    /// Closing doesn't wait for the results: typing on would drop a close left waiting.
    @objc func quickLook(_ sender: Any?) {
        if previewing { return closePreview() }
        onceAnswered { [weak self] in
            guard let self, !composing, let row = selected, found(row) else { return }
            // In front but not key, so typing and the arrows stay in the box.
            inPreview = row.url
            closing = false
            stage.openPreview()
        }
    }

    @objc func toggleDetails(_ sender: Any?) {
        detailsOn.toggle()
        if detailsOn { showDetails() } else { pane.stopThumbnail() }
        fit()
    }

    private func showDetails() {
        guard detailsOn else { return }
        pane.show(selected, scale: panel.backingScaleFactor)
    }

    var previewing: Bool {
        !closing && stage.previewShows
    }

    /// Quick Look takes about 0.2 s to close and stays visible meanwhile; a second close in
    /// that time cancels the first and leaves it open (macOS 26), so Esc twice would keep it up.
    func closePreview() {
        guard previewing else { return }
        closing = true
        stage.closePreview()
    }

    /// The preview moves with the selection, and closes once nothing is selected. It closes
    /// quietly on a file gone since the list was built; "can't be found" is for ⌘Y and Return.
    private func follow() {
        guard previewing else { return }
        guard let row = selected, FileManager.default.fileExists(atPath: row.path) else {
            return closePreview()
        }
        guard row.url != inPreview else { return }
        inPreview = row.url
        stage.reloadPreview()
    }

    /// A click on the preview gives it the keys and closing it hands them back to the box; only
    /// a click in another app hides the window.
    func settleKeys() {
        DispatchQueue.main.async { [weak self] in
            guard let self, shown, !stage.isKey(panel), !stage.previewIsKey else { return }
            guard stage.isActive else { return hide() }
            stage.makeKey(panel)
            if field.currentEditor() == nil { panel.makeFirstResponder(field) }
        }
    }

    func numberOfPreviewItems(in panel: QLPreviewPanel!) -> Int {
        selected == nil ? 0 : 1
    }

    func previewPanel(_ panel: QLPreviewPanel!, previewItemAt index: Int) -> QLPreviewItem! {
        selected?.real as NSURL?
    }

    /// The arrows still pick the next result while the preview has the keys.
    func previewPanel(_ panel: QLPreviewPanel!, handle event: NSEvent!) -> Bool {
        guard event.type == .keyDown, event.modifierFlags.isDisjoint(with: [.command, .option, .control]) else {
            return false
        }
        switch event.specialKey {
        case .downArrow: move(by: 1)
        case .upArrow: move(by: -1)
        default: return false
        }
        return true
    }

    func controlTextDidChange(_ note: Notification) {
        search(typedAt: pressedAt())
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        switch selector {
        case #selector(NSResponder.moveDown(_:)):
            move(by: 1)
        case #selector(NSResponder.moveUp(_:)):
            move(by: -1)
        case #selector(NSResponder.cancelOperation(_:)):
            // The first Esc clears the box, the second hides the window. A preview closes first.
            if previewing {
                closePreview()
            } else if field.stringValue.isEmpty {
                hide()
            } else {
                field.stringValue = ""
                search()
            }
        case #selector(NSResponder.insertNewline(_:)):
            onceAnswered { [weak self] in self?.open(revealing: false) }
        case #selector(NSResponder.insertTab(_:)) where tabIntoFolder():
            break
        case #selector(NSResponder.deleteBackward(_:)):
            return leaveFolder()
        case #selector(NSResponder.insertTab(_:)), #selector(NSResponder.insertBacktab(_:)):
            // With nowhere to go, Tab would move the keys from the box to the list.
            return reachSettings() || !composing
        default:
            return false
        }
        return true
    }

    /// Quick Look takes the keys as it comes up, even while you're in another app, and macOS
    /// then doesn't count it as key, so the window would hide. Only a click on it keeps them.
    func windowDidBecomeKey(_ note: Notification) {
        let clicked = NSApp.currentEvent.map { $0.type == .leftMouseDown && $0.window === note.object as? NSWindow }
        guard note.object as? NSWindow !== panel, !stage.isActive, clicked != true else { return }
        stage.makeKey(panel)
    }

    /// Also hears the preview, whose delegate this is while it shows.
    func windowDidResignKey(_ note: Notification) {
        guard note.object as? NSWindow === panel, !previewing else { return settleKeys() }
        hide()
    }

    /// Arrows, a click on a row, and a new list all land here.
    func tableViewSelectionDidChange(_ note: Notification) {
        showDetails()
        follow()
    }

    /// A click below the last row or a ⌘-click keeps the pick, so Return, ⌘Y and the pane still
    /// have a file.
    func tableView(_ tableView: NSTableView, selectionIndexesForProposedSelection proposed: IndexSet) -> IndexSet {
        var proposed = proposed
        if let header { proposed.remove(header) }
        return proposed.isEmpty ? tableView.selectedRowIndexes : proposed
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        listed
    }

    func tableView(_ tableView: NSTableView, heightOfRow row: Int) -> CGFloat {
        row == header ? Metric.header : Metric.row
    }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        AccentRow()
    }

    func tableView(_ tableView: NSTableView, viewFor column: NSTableColumn?, row: Int) -> NSView? {
        let cell = tableView.makeView(withIdentifier: ResultCell.id, owner: nil) as? ResultCell ?? ResultCell()
        show(row, in: cell)
        return cell
    }

    private func show(_ row: Int, in cell: ResultCell) {
        if rows.indices.contains(row) {
            cell.show(rows[row], icon: icon(for: rows[row]))
        } else if row == header {
            cell.showHeader("Inside Files")
        } else if let hit = hit(at: row) {
            cell.show(hit, icon: icon(for: hit.file))
        }
    }

    /// reloadData throws every row away and builds it again, most of the wait after a keystroke.
    private func refill() {
        table.noteNumberOfRowsChanged()
        // The table keeps the old heights through noteNumberOfRowsChanged, so the title's old
        // row would stay short.
        if shownHeader != header {
            let moved = IndexSet([shownHeader, header].compactMap { $0 }.filter { $0 < listed })
            shownHeader = header
            table.noteHeightOfRows(withIndexesChanged: moved)
        }
        table.enumerateAvailableRowViews { view, row in
            guard let cell = view.view(atColumn: 0) as? ResultCell, row < listed else { return }
            show(row, in: cell)
        }
    }

    private func icon(for row: Row) -> NSImage {
        if row.isApp {
            let scale = panel.backingScaleFactor
            // An app updated with a new icon, or found while still being copied in, is drawn again.
            var info = stat()
            let changed = stat(row.path, &info) == 0 ? info.st_mtimespec.tv_sec : 0
            let own = "\(row.path) \(scale) \(changed)"
            if let drawn = icons[own] { return drawn }
            readIcon(of: row.path, as: own, scale: scale)
            return kindIcon(".app", .applicationBundle)
        }
        if row.isFolder { return kindIcon("/", .folder) }
        let ext = (row.name as NSString).pathExtension.lowercased()
        return kindIcon(ext, UTType(filenameExtension: ext) ?? .data)
    }

    /// ".app" and "/" are never a file's extension.
    private func kindIcon(_ key: String, _ type: @autoclosure () -> UTType) -> NSImage {
        if let cached = icons[key] { return cached }
        let image = rowIcon(NSWorkspace.shared.icon(for: type()))
        icons[key] = image
        return image
    }

    /// Drawn here, an app's icon took up to 140 ms the first time and macOS kept about 11 MB in
    /// Telescope for 98 apps. Quick Look draws it in its own process at the row's size; until
    /// then, or if it can't, the row shows the icon every app shares.
    private func readIcon(of path: String, as key: String, scale: CGFloat) {
        guard readingIcons.insert(key).inserted else { return }
        Task { [weak self] in
            let drawn = await appIcon(at: path, scale: scale)
            guard let self else { return }
            readingIcons.remove(key)
            guard let drawn else { return }
            let image = NSImage(cgImage: drawn, size: NSSize(width: 32, height: 32))
            icons[key] = image
            table.enumerateAvailableRowViews { view, row in
                guard rows.indices.contains(row), rows[row].path == path,
                    let cell = view.view(atColumn: 0) as? ResultCell
                else { return }
                cell.show(rows[row], icon: image)
            }
        }
    }
}

/// Handed the icon itself, a row's image view asks macOS for a stand-in picture each time, most
/// of the time spent filling the rows. Drawn from a copy, it doesn't.
private func rowIcon(_ system: NSImage) -> NSImage {
    NSImage(size: NSSize(width: 32, height: 32), flipped: false) { @Sendable rect in
        system.draw(in: rect)
        return true
    }
}

/// The app's icon at a row's size, as Finder shows it. /Applications/Safari.app is a link into
/// the system's own volume, and read at the link Quick Look draws the link's icon instead.
private nonisolated func appIcon(at path: String, scale: CGFloat) async -> CGImage? {
    await quickLookPicture(of: URL(fileURLWithPath: path).resolvingSymlinksInPath(), side: 32, scale: scale, .icon)
}

/// When the key behind the current event went down, so timings include the wait to reach us.
/// The hotkey arrives as a system-defined event; anything else times from now.
@MainActor
private func pressedAt() -> TimeInterval {
    guard let event = NSApp.currentEvent, [.keyDown, .systemDefined].contains(event.type) else {
        return ProcessInfo.processInfo.systemUptime
    }
    return event.timestamp
}

private func ms(since start: TimeInterval) -> Double {
    (ProcessInfo.processInfo.systemUptime - start) * 1000
}
