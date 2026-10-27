import AppKit
import OSLog
import UniformTypeIdentifiers

enum Metric {
    static let width: CGFloat = 680
    static let field: CGFloat = 56
    static let row: CGFloat = 48
    static let visibleRows = 8
    static let inset: CGFloat = 8
    static let message: CGFloat = 48
    static let fade: TimeInterval = 0.12
    static let resize: TimeInterval = 0.15
}

/// Read with `log show --last 5m --predicate 'subsystem == "io.github.jovanhartono.telescope"'`.
let timing = Logger(subsystem: "io.github.jovanhartono.telescope", category: "timing")
let failures = Logger(subsystem: "io.github.jovanhartono.telescope", category: "failure")

final class Panel: NSPanel {
    override var canBecomeKey: Bool { true }

    /// Esc while Open Settings has the focus; the box handles its own Esc.
    override func cancelOperation(_ sender: Any?) {
        (delegate as? SearchPanel)?.hide()
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
    NSTableViewDelegate
{
    let panel = Panel(
        contentRect: NSRect(x: 0, y: 0, width: Metric.width, height: Metric.field),
        styleMask: [.titled, .fullSizeContentView, .nonactivatingPanel],
        backing: .buffered, defer: false)
    private let glass = NSImageView(frame: NSRect(x: 18, y: 16, width: 24, height: 24))
    let field = NSTextField(frame: NSRect(x: 50, y: 13, width: Metric.width - 68, height: 30))
    private let line = NSBox(frame: NSRect(x: 0, y: Metric.field, width: Metric.width, height: 1))
    private let scroll = NSScrollView()
    private let table = NSTableView()
    private let message = NSTextField(labelWithString: "")
    private let bar = TopDown()
    private let barLine = NSBox(frame: NSRect(x: 0, y: 0, width: Metric.width, height: 1))
    let barText = NSTextField(labelWithString: "")
    let settings = KeyButton(title: "Open Settings", target: nil, action: nil)

    private var hatch: Hatch?
    private let hasAccess: () -> Bool
    private(set) var notice: Notice?
    private var indexingSince: TimeInterval = 0
    private(set) var indexed = false
    private var problem: String?
    /// The file you picked was moved or deleted after the list was built.
    private var missing: String?
    private var newest: UInt64 = 0
    private var sentAt: [UInt64: TimeInterval] = [:]
    private(set) var rows: [Row] = []
    /// The list changed while the window was hidden, so the results on screen may be old.
    private(set) var stale = false
    /// A search run again on a new list, and the file to keep selected when it answers.
    private var rerun: (ticket: UInt64, keeping: String?)?
    /// The ticket the rows on screen answer; 0 before any answer, so an empty list isn't "No Results".
    private var answered: UInt64 = 0
    private var pending: (() -> Void)?
    private var shown = false
    private var icons: [String: NSImage] = [:]

    init(root: String = NSHomeDirectory(), hasAccess: @escaping () -> Bool = { Hatch.hasFullDiskAccess() }) {
        self.hasAccess = hasAccess
        super.init()
        build()
        let onEvent: (Hatch.Event) -> Void = { [weak self] event in self?.heard(event) }
        // TELESCOPE_STAND_IN=1000000 searches made-up names instead of your files, and 0 measures
        // the window's own memory without any list.
        indexingSince = ProcessInfo.processInfo.systemUptime
        if let count = ProcessInfo.processInfo.environment["TELESCOPE_STAND_IN"].flatMap(UInt64.init) {
            hatch = Hatch(standIn: count, onEvent: onEvent)
        } else {
            let access = hasAccess()
            post(access ? nil : .needsAccess)
            hatch = Hatch(root: root, fullDisk: access, onEvent: onEvent)
        }
        if hatch == nil { problem = "Search couldn't start. Rebuild Telescope." }
        fit()
    }

    func toggle() {
        shown ? hide() : show()
    }

    /// The very first show costs 30-50 ms, so it happens once at launch, out of sight.
    func warmUp() {
        panel.alphaValue = 0
        place()
        panel.orderFrontRegardless()
        panel.display()
        panel.orderOut(nil)
    }

    func show() {
        let pressed = pressedAt()
        recheckAccess()
        if stale { runAgain() }
        shown = true
        place()
        panel.alphaValue = reduceMotion ? 1 : 0
        panel.orderFrontRegardless()
        panel.makeKey()
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
        // A word left half-typed when you click away is kept as typed, so searching isn't
        // blocked once you're back.
        if composing, let editor = field.currentEditor() as? NSTextView {
            editor.inputContext?.discardMarkedText()
            editor.unmarkText()
            search()
        }
        fade(to: 0) { [weak self] in
            // Shown again while fading out: keep it.
            if self?.shown == false { self?.panel.orderOut(nil) }
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

        line.boxType = .separator

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
        for view in [barLine, barText, settings] { bar.addSubview(view) }
        bar.frame = NSRect(x: 0, y: Metric.field, width: Metric.width, height: 1 + Metric.message)
        bar.isHidden = true
        field.nextKeyView = settings
        settings.nextKeyView = field

        let content = TopDown()
        for view in [glass, field, line, scroll, message, bar] { content.addSubview(view) }
        panel.contentView = content
    }

    /// Checked on each show while access is missing, so the line goes once access is switched
    /// on in System Settings and Later is clicked, and the guarded folders come in.
    func recheckAccess() {
        guard notice == .needsAccess, hasAccess() else { return }
        post(nil)
        hatch?.grantFullDisk()
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
        hide()
        NSWorkspace.shared.open(Notice.settings)
    }

    /// A fifth of the way down the screen that has the mouse.
    private func place() {
        let mouse = NSEvent.mouseLocation
        let screen = NSScreen.screens.first { NSMouseInRect(mouse, $0.frame, false) } ?? .main
        guard let visible = screen?.visibleFrame else { return }
        let height = panel.frame.height
        let top = visible.maxY - visible.height * 0.2
        let frame = NSRect(
            x: visible.midX - Metric.width / 2, y: top - height, width: Metric.width, height: height)
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

    private var status: String? {
        if let problem { return problem }
        if composing { return nil }
        if !indexed { return "Getting ready…" }
        if let missing { return missing }
        if answered != 0 && rows.isEmpty { return "No Results" }
        return nil
    }

    private var bodyHeight: CGFloat {
        if field.stringValue.isEmpty { return 0 }
        if status != nil { return Metric.message }
        if rows.isEmpty { return 0 }
        return CGFloat(min(rows.count, Metric.visibleRows)) * Metric.row + 2 * Metric.inset
    }

    /// The box alone while it is empty; it grows downward to fit up to 8 rows.
    private func fit() {
        let body = bodyHeight
        let status = self.status
        message.stringValue = status ?? ""
        line.isHidden = body == 0
        scroll.isHidden = body == 0 || status != nil
        message.isHidden = body == 0 || status == nil
        scroll.frame = NSRect(x: 0, y: head + 1, width: Metric.width, height: body)

        let height = head + (body > 0 ? 1 + body : 0)
        var frame = panel.frame
        guard frame.height != height else { return }
        frame.origin.y += frame.height - height
        frame.size.height = height
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
        case .indexed(let count):
            timing.notice("indexed \(count) entries in \(ms(since: self.indexingSince), format: .fixed(precision: 0)) ms")
            indexed = true
            hatch?.watch()
            search()
        case .rebuilt(let count, let took):
            timing.notice("rebuilt \(count) entries in \(took) ms after a change")
            hatch?.watch()
            if shown { runAgain() } else { stale = true }
        case .ready(let ticket) where ticket == newest:
            take(ticket)
        case .ready:
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
        if field.stringValue.isEmpty {
            // A reply to what was typed before the box was cleared must not fill it again.
            newest = 0
            sentAt = [:]
            rows = []
            answered = 0
            table.reloadData()
        }
        guard let hatch, indexed, !composing, !field.stringValue.isEmpty else { return fit() }
        newest = hatch.submit(field.stringValue)
        sentAt[newest] = typed
        fit()
    }

    /// The same search on a new list. No keystroke started it, so it writes no timing line.
    private func runAgain() {
        stale = false
        guard let hatch, indexed, !composing, !field.stringValue.isEmpty else { return }
        newest = hatch.submit(field.stringValue)
        rerun = (newest, selected?.path)
    }

    private func take(_ ticket: UInt64) {
        guard let fresh = hatch?.take(ticket) else { return }
        // A file you picked stays picked when a change moves it up or down the list.
        let keeping = rerun?.ticket == ticket ? rerun?.keeping : nil
        rerun = nil
        rows = fresh
        answered = ticket
        table.reloadData()
        if !rows.isEmpty { select(rows.firstIndex { $0.path == keeping } ?? 0) }
        fit()
        let typed = sentAt.removeValue(forKey: ticket)
        sentAt = sentAt.filter { $0.key > ticket }
        if let action = pending {
            pending = nil
            action()
        } else if let typed {
            // The rows grow into view over the 0.15 s resize; this times their first frame.
            panel.displayIfNeeded()
            timing.notice("results drawn \(ms(since: typed), format: .fixed(precision: 2)) ms after the keystroke")
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
        if !rows.isEmpty { select(min(max(table.selectedRow + step, 0), rows.count - 1)) }
    }

    var selected: Row? {
        rows.indices.contains(table.selectedRow) ? rows[table.selectedRow] : nil
    }

    /// Japanese and Chinese input pick the half-typed word with Return, so nothing opens then.
    private func open(revealing reveal: Bool) {
        guard !composing, let row = selected else { return }
        guard FileManager.default.fileExists(atPath: row.path) else {
            missing = "“\(row.name)” can’t be found."
            return fit()
        }
        hide()
        let url = URL(fileURLWithPath: row.path)
        if reveal {
            NSWorkspace.shared.activateFileViewerSelecting([url])
        } else {
            NSWorkspace.shared.open(url)
        }
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
            // The first Esc clears the box, the second hides the window.
            if field.stringValue.isEmpty {
                hide()
            } else {
                field.stringValue = ""
                search()
            }
        case #selector(NSResponder.insertNewline(_:)):
            onceAnswered { [weak self] in self?.open(revealing: false) }
        case #selector(NSResponder.insertTab(_:)), #selector(NSResponder.insertBacktab(_:)):
            guard notice != nil else { return false }
            panel.makeFirstResponder(settings)
        default:
            return false
        }
        return true
    }

    func windowDidResignKey(_ note: Notification) {
        hide()
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        rows.count
    }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        AccentRow()
    }

    func tableView(_ tableView: NSTableView, viewFor column: NSTableColumn?, row: Int) -> NSView? {
        let cell = tableView.makeView(withIdentifier: ResultCell.id, owner: nil) as? ResultCell ?? ResultCell()
        let item = rows[row]
        cell.name.stringValue = item.name
        cell.folder.stringValue = item.folder
        cell.icon.image = icon(for: item)
        return cell
    }

    private func icon(for row: Row) -> NSImage {
        let key = row.isFolder ? "/" : (row.name as NSString).pathExtension.lowercased()
        if let cached = icons[key] { return cached }
        let type = row.isFolder ? UTType.folder : (UTType(filenameExtension: key) ?? .data)
        let image = NSWorkspace.shared.icon(for: type)
        icons[key] = image
        return image
    }
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
