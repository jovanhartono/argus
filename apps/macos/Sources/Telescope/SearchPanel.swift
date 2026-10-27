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
    /// How many made-up names the stand-in core holds until M2 reads your disk.
    static let standIn: UInt64 = 1_000_000
}

/// Read with `log show --last 5m --predicate 'subsystem == "io.github.jovanhartono.telescope"'`.
let timing = Logger(subsystem: "io.github.jovanhartono.telescope", category: "timing")

final class Panel: NSPanel {
    override var canBecomeKey: Bool { true }
}

/// Lays out from the top, so the box stays put while the window grows or shrinks below it.
final class TopDown: NSView {
    override var isFlipped: Bool { true }
}

@MainActor
final class SearchPanel: NSObject, NSWindowDelegate, NSTextFieldDelegate, NSTableViewDataSource,
    NSTableViewDelegate
{
    private let panel = Panel(
        contentRect: NSRect(x: 0, y: 0, width: Metric.width, height: Metric.field),
        styleMask: [.titled, .fullSizeContentView, .nonactivatingPanel],
        backing: .buffered, defer: false)
    private let glass = NSImageView(frame: NSRect(x: 18, y: 16, width: 24, height: 24))
    private let field = NSTextField(frame: NSRect(x: 50, y: 13, width: Metric.width - 68, height: 30))
    private let line = NSBox(frame: NSRect(x: 0, y: Metric.field, width: Metric.width, height: 1))
    private let scroll = NSScrollView()
    private let table = NSTableView()
    private let message = NSTextField(labelWithString: "")

    private var hatch: Hatch?
    private var indexed = false
    private var problem: String?
    private var newest: UInt64 = 0
    private var sentAt: [UInt64: CFAbsoluteTime] = [:]
    private var rows: [Row] = []
    /// The rows on screen answer the text in the box, so an empty list means "No Results".
    private var answered = false
    private var shown = false
    private var icons: [String: NSImage] = [:]

    override init() {
        super.init()
        build()
        hatch = Hatch(standIn: Metric.standIn) { [weak self] event in self?.heard(event) }
        if hatch == nil { problem = "Search couldn't start. Rebuild Telescope." }
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
        let started = CFAbsoluteTimeGetCurrent()
        shown = true
        place()
        panel.alphaValue = reduceMotion ? 1 : 0
        panel.orderFrontRegardless()
        panel.makeKey()
        panel.makeFirstResponder(field)
        // Your last search comes back selected, so typing replaces it.
        field.selectText(nil)
        panel.displayIfNeeded()
        timing.notice("window shown in \(ms(since: started), format: .fixed(precision: 2)) ms")
        fade(to: 1)
    }

    func hide() {
        guard shown else { return }
        shown = false
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
        message.frame = NSRect(
            x: 16, y: Metric.field + 1 + (Metric.message - 17) / 2, width: Metric.width - 32,
            height: 17)

        let content = TopDown()
        for view in [glass, field, line, scroll, message] { content.addSubview(view) }
        panel.contentView = content
        fit()
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
        if answered && rows.isEmpty { return "No Results" }
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
        scroll.frame = NSRect(x: 0, y: Metric.field + 1, width: Metric.width, height: body)

        let height = Metric.field + (body > 0 ? 1 + body : 0)
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
        case .indexed:
            indexed = true
            search()
        case .ready(let ticket) where ticket == newest:
            take(ticket)
        case .ready:
            break
        case .failed:
            problem = "Search stopped working. Quit and reopen Telescope."
            fit()
        }
    }

    private func search() {
        if field.stringValue.isEmpty {
            // A reply to what was typed before the box was cleared must not fill it again.
            newest = 0
            sentAt = [:]
            rows = []
            answered = false
            table.reloadData()
        }
        guard let hatch, indexed, !composing, !field.stringValue.isEmpty else { return fit() }
        newest = hatch.submit(field.stringValue)
        sentAt[newest] = CFAbsoluteTimeGetCurrent()
        fit()
    }

    private func take(_ ticket: UInt64) {
        guard let fresh = hatch?.take(ticket) else { return }
        rows = fresh
        answered = true
        table.reloadData()
        if !rows.isEmpty { select(0) }
        fit()
        if let sent = sentAt.removeValue(forKey: ticket) {
            timing.notice("results in \(ms(since: sent), format: .fixed(precision: 2)) ms")
        }
        sentAt = sentAt.filter { $0.key > ticket }
    }

    private func select(_ row: Int) {
        table.selectRowIndexes([row], byExtendingSelection: false)
        table.scrollRowToVisible(row)
    }

    func controlTextDidChange(_ note: Notification) {
        search()
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        switch selector {
        case #selector(NSResponder.moveDown(_:)):
            if !rows.isEmpty { select(min(table.selectedRow + 1, rows.count - 1)) }
        case #selector(NSResponder.moveUp(_:)):
            if !rows.isEmpty { select(max(table.selectedRow - 1, 0)) }
        case #selector(NSResponder.cancelOperation(_:)):
            // The first Esc clears the box, the second hides the window.
            if field.stringValue.isEmpty {
                hide()
            } else {
                field.stringValue = ""
                search()
            }
        case #selector(NSResponder.insertNewline(_:)):
            break  // Opening the selected file comes in step 3.
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

private func ms(since start: CFAbsoluteTime) -> Double {
    (CFAbsoluteTimeGetCurrent() - start) * 1000
}
