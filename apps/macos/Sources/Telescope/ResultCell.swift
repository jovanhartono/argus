import AppKit

/// Shows a file found by name, a line found inside a file, or the title above those lines. A
/// row on screen changes from one to another in place as you type.
final class ResultCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("result")

    let icon = NSImageView(frame: NSRect(x: 6, y: 8, width: 32, height: 32))
    let name = NSTextField(labelWithString: "")
    let folder = NSTextField(labelWithString: "")
    private(set) var hit: TextHit?
    private var shown = ("", NSRange())
    private(set) var isHeader = false

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
        if hit != nil || isHeader { reset() }
        if name.stringValue != row.name { name.stringValue = row.name }
        let place = Self.place(row.folder)
        if folder.stringValue != place { folder.stringValue = place }
        if icon.image !== image { icon.image = image }
    }

    private static let home = NSHomeDirectory()

    /// A folder on a drive leads with the drive's name, "Photos › trips/2024", kept in view
    /// when a long path is cut in the middle.
    static func place(_ folder: String, home: String = ResultCell.home) -> String {
        guard folder.hasPrefix("/Volumes/"), !folder.isInside(home) else { return folder }
        let parts = folder.dropFirst("/Volumes/".count).split(separator: "/", maxSplits: 1)
        guard let drive = parts.first else { return folder }
        return parts.count == 1 ? String(drive) : "\(drive) › \(parts[1])"
    }

    /// "budget.md, line 12 · ~/notes" under the line, the match in bold.
    func show(_ found: TextHit, icon image: NSImage) {
        if isHeader { reset() }
        guard found != hit else { return }
        hit = found
        name.lineBreakMode = .byTruncatingTail
        shown = Self.excerpt(found.text, match: found.match, width: name.frame.width, font: Self.bold)
        paint()
        let file = found.file
        folder.stringValue = "\(file.name), line \(found.line) · \(Self.place((file.folder as NSString).abbreviatingWithTildeInPath))"
        if icon.image !== image { icon.image = image }
    }

    func showHeader(_ title: String) {
        guard !isHeader else { return }
        hit = nil
        isHeader = true
        icon.isHidden = true
        folder.isHidden = true
        name.frame.origin = NSPoint(x: 18, y: 4)
        name.font = .systemFont(ofSize: NSFont.smallSystemFontSize, weight: .semibold)
        name.textColor = .secondaryLabelColor
        name.stringValue = title
    }

    private func reset() {
        hit = nil
        isHeader = false
        icon.isHidden = false
        folder.isHidden = false
        name.frame.origin = NSPoint(x: 48, y: 24)
        name.font = .systemFont(ofSize: NSFont.systemFontSize)
        name.lineBreakMode = .byTruncatingMiddle
        name.stringValue = ""
        folder.stringValue = ""
        backgroundStyle = backgroundStyle
    }

    /// An attributed line keeps the colour it was made with, so it is made again on selection.
    private func paint() {
        guard hit != nil else { return }
        let (text, match) = shown
        let color = name.textColor ?? .labelColor
        let line = NSMutableAttributedString(
            string: text, attributes: [.font: NSFont.systemFont(ofSize: NSFont.systemFontSize), .foregroundColor: color])
        if match.length > 0, NSMaxRange(match) <= line.length {
            line.addAttribute(.font, value: Self.bold, range: match)
        }
        name.attributedStringValue = line
    }

    private static let bold = NSFont.boldSystemFont(ofSize: NSFont.systemFontSize)

    /// A line up to 200 bytes comes whole, wider than the row; its end is cut off, so a match
    /// near the end would be too. Then the line starts a few words before the match instead.
    static func excerpt(_ text: String, match: NSRange, width: CGFloat, font: NSFont) -> (String, NSRange) {
        let line = text as NSString
        let lead = 24
        // Measuring takes about 50 µs a line; this few letters fit in any row.
        guard NSMaxRange(match) <= line.length, NSMaxRange(match) > lead + 16,
            line.substring(to: NSMaxRange(match)).size(withAttributes: [.font: font]).width > width
        else { return (text, match) }
        var start = line.rangeOfComposedCharacterSequence(at: max(0, match.location - lead)).location
        let space = line.range(of: " ", range: NSRange(location: start, length: match.location - start))
        if space.location != NSNotFound { start = NSMaxRange(space) }
        guard start > 0 else { return (text, match) }
        return ("…" + line.substring(from: start), NSRange(location: match.location - start + 1, length: match.length))
    }

    override var backgroundStyle: NSView.BackgroundStyle {
        didSet {
            guard !isHeader else { return }
            let selected = backgroundStyle == .emphasized
            name.textColor = selected ? .alternateSelectedControlTextColor : .labelColor
            folder.textColor =
                selected
                ? .alternateSelectedControlTextColor.withAlphaComponent(0.8) : .secondaryLabelColor
            paint()
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
