import AppKit

final class ResultCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("result")

    let icon = NSImageView(frame: NSRect(x: 6, y: 8, width: 32, height: 32))
    let name = NSTextField(labelWithString: "")
    let folder = NSTextField(labelWithString: "")

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: Metric.width, height: Metric.row))
        identifier = Self.id
        name.frame = NSRect(x: 48, y: 24, width: Metric.width - 80, height: 17)
        name.font = .systemFont(ofSize: NSFont.systemFontSize)
        folder.frame = NSRect(x: 48, y: 8, width: Metric.width - 80, height: 15)
        folder.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        for label in [name, folder] {
            label.lineBreakMode = .byTruncatingMiddle
            label.autoresizingMask = [.width]
            addSubview(label)
        }
        addSubview(icon)
        backgroundStyle = .normal
    }

    required init?(coder: NSCoder) { nil }

    /// Only what differs is set: each label set again lays itself out again.
    func show(_ row: Row, icon image: NSImage) {
        if name.stringValue != row.name { name.stringValue = row.name }
        if folder.stringValue != row.folder { folder.stringValue = row.folder }
        if icon.image !== image { icon.image = image }
    }

    override var backgroundStyle: NSView.BackgroundStyle {
        didSet {
            let selected = backgroundStyle == .emphasized
            name.textColor = selected ? .alternateSelectedControlTextColor : .labelColor
            folder.textColor =
                selected
                ? .alternateSelectedControlTextColor.withAlphaComponent(0.8) : .secondaryLabelColor
        }
    }
}

/// The panel is the key window whenever it is on screen, so the selection always takes the
/// accent colour, inset and rounded like AppKit's own inset lists.
final class AccentRow: NSTableRowView {
    override var isEmphasized: Bool {
        get { true }
        set {}
    }

    override func drawSelection(in dirtyRect: NSRect) {
        NSColor.selectedContentBackgroundColor.setFill()
        let pill = bounds.insetBy(dx: Metric.inset, dy: 0)
        NSBezierPath(roundedRect: pill, xRadius: 8, yRadius: 8).fill()
    }
}
