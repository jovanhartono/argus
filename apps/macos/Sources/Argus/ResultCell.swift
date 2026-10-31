import AppKit

/// Shows a file found by name or by its text, a line found inside a file under that file, or
/// the title above those. A row on screen changes from one to another in place as you type.
final class ResultCell: NSTableCellView, NSViewToolTipOwner {
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
        addToolTip(bounds, owner: self, userData: nil)
    }

    required init?(coder: NSCoder) { nil }

    /// A name or folder cut short shows whole on hover. Worked out only then: AppKit's own
    /// expansion tips measured every label as it was set, 1.5-2 ms more per keystroke.
    func view(
        _ view: NSView, stringForToolTip tag: NSView.ToolTipTag, point: NSPoint, userData data: UnsafeMutableRawPointer?
    ) -> String {
        let label = point.y >= name.frame.minY ? name : folder
        guard !label.isHidden, label.cell!.cellSize.width > label.frame.width else { return "" }
        return label.stringValue
    }

    /// Only what differs is set: each label set again lays itself out again.
    /// Inside `scope`, the folder the chip names, the folder is written from there.
    func show(_ row: Row, icon image: NSImage, in scope: String? = nil) {
        if hit != nil || isHeader { reset() }
        if name.stringValue != row.name { name.stringValue = row.name }
        let place = Self.place(row.folder, in: scope)
        if folder.stringValue != place { folder.stringValue = place }
        if icon.image !== image { icon.image = image }
    }

    private static let home = NSHomeDirectory()

    /// The home folder is "~", as the details pane writes it. A folder on a drive leads with the
    /// drive's name, "Photos › trips/2024", and one inside the chip's folder with that folder's
    /// name, "Budget › invoices", each kept in view when a long path is cut in the middle.
    static func place(_ folder: String, in scope: String? = nil, home: String = ResultCell.home) -> String {
        if let scope, folder.isInside(scope) {
            let name = (scope as NSString).lastPathComponent
            return folder == scope ? name : "\(name) › \(folder.dropFirst(scope.count).drop { $0 == "/" })"
        }
        if folder.isInside(home) { return "~" + folder.dropFirst(home.count) }
        guard folder.hasPrefix("/Volumes/") else { return folder }
        let parts = folder.dropFirst("/Volumes/".count).split(separator: "/", maxSplits: 1)
        guard let drive = parts.first else { return folder }
        return parts.count == 1 ? String(drive) : "\(drive) › \(parts[1])"
    }

    /// "12  Budget for 2026" under its file's row, half as tall: the line number in grey, the
    /// match in bold.
    func show(line found: TextHit) {
        if isHeader { reset() }
        guard found != hit else { return }
        if hit == nil {
            icon.isHidden = true
            folder.frame = NSRect(x: 40, y: 5, width: 40, height: 14)
            folder.autoresizingMask = []
            folder.alignment = .right
            folder.font = .monospacedDigitSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
            name.frame = NSRect(x: 88, y: 4, width: bounds.width - 120, height: 17)
            name.lineBreakMode = .byTruncatingTail
        }
        hit = found
        shown = Self.excerpt(found.text, match: found.match, width: name.frame.width, font: Self.bold)
        paint()
        folder.stringValue = String(found.line)
        setAccessibilityLabel("line \(found.line): \(found.text)")
    }

    /// The title's words change as the search for lines goes on.
    func showHeader(_ title: String) {
        if !isHeader {
            hit = nil
            isHeader = true
            setAccessibilityLabel(nil)
            icon.isHidden = true
            folder.isHidden = true
            name.frame.origin = NSPoint(x: 18, y: 4)
            name.font = .systemFont(ofSize: NSFont.smallSystemFontSize, weight: .semibold)
            name.textColor = .secondaryLabelColor
        }
        if name.stringValue != title { name.stringValue = title }
    }

    private func reset() {
        hit = nil
        isHeader = false
        icon.isHidden = false
        folder.isHidden = false
        name.frame = NSRect(x: 48, y: 24, width: bounds.width - 80, height: 17)
        folder.frame = NSRect(x: 48, y: 8, width: bounds.width - 80, height: 15)
        folder.autoresizingMask = [.width]
        name.font = .systemFont(ofSize: NSFont.systemFontSize)
        folder.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        folder.alignment = .natural
        name.lineBreakMode = .byTruncatingMiddle
        name.stringValue = ""
        folder.stringValue = ""
        setAccessibilityLabel(nil)
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

/// The panel is the key window whenever it is on screen, so the selection takes the accent
/// colour, inset and rounded like AppKit's own inset lists.
final class AccentRow: NSTableRowView {
    /// Gray while the keys are on the other list: the results' or the details pane's.
    var muted = false {
        didSet {
            guard muted != oldValue else { return }
            needsDisplay = true
            for case let cell as NSTableCellView in subviews { cell.backgroundStyle = interiorBackgroundStyle }
        }
    }
    var inset = Metric.inset

    override var isEmphasized: Bool {
        get { !muted }
        set {}
    }

    override func drawSelection(in dirtyRect: NSRect) {
        (muted ? NSColor.unemphasizedSelectedContentBackgroundColor : .selectedContentBackgroundColor).setFill()
        let pill = bounds.insetBy(dx: inset, dy: 0)
        NSBezierPath(roundedRect: pill, xRadius: 8, yRadius: 8).fill()
    }
}
