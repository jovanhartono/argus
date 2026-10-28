import AppKit
import QuickLookThumbnailing
import UniformTypeIdentifiers

/// What the pane says about one file, read from disk only for the selected row.
struct Details: Equatable {
    let type: UTType
    /// Finder's own word for the kind, such as "Folder" or "PDF document"; nil once the file is gone.
    var kind: String? = nil
    /// Nil for a folder: adding up what's inside can take seconds.
    let size: Int64?
    let created: Date?
    let modified: Date?
    /// False once the file was moved or deleted after the list was built.
    let found: Bool

    static func read(_ row: Row) -> Details {
        let keys: Set<URLResourceKey> = [
            .contentTypeKey, .localizedTypeDescriptionKey, .fileSizeKey, .creationDateKey, .contentModificationDateKey,
        ]
        guard let values = try? row.url.resourceValues(forKeys: keys) else {
            let guess = row.isFolder ? UTType.folder : UTType(filenameExtension: (row.name as NSString).pathExtension)
            return Details(type: guess ?? .data, size: nil, created: nil, modified: nil, found: false)
        }
        return Details(
            type: values.contentType ?? (row.isFolder ? .folder : .data), kind: values.localizedTypeDescription,
            size: row.isFolder ? nil : values.fileSize.map(Int64.init),
            created: values.creationDate, modified: values.contentModificationDate, found: true)
    }
}

/// Sizes and dates the way Finder writes them, in your language and time zone.
final class DetailsFormat {
    private let dates = DateFormatter()
    /// Finder's "12 KB"; the newer format style writes "kB" and rounds 999 bytes up to 1 kB.
    private let sizes = ByteCountFormatter()

    init(locale: Locale = .autoupdatingCurrent, timeZone: TimeZone = .autoupdatingCurrent) {
        dates.locale = locale
        dates.timeZone = timeZone
        dates.dateStyle = .medium
        dates.timeStyle = .short
        dates.doesRelativeDateFormatting = true
        sizes.countStyle = .file
        sizes.allowsNonnumericFormatting = false
    }

    /// "PDF document – 1.2 MB", as under a file's name in Finder's preview.
    func summary(_ details: Details) -> String {
        let kind = details.kind ?? Self.capitalized(details.type.localizedDescription ?? "document")
        if !details.found { return "\(kind) – can’t be found" }
        guard let size = details.size else { return kind }
        return "\(kind) – \(sizes.string(fromByteCount: size))"
    }

    /// macOS's type names start lower case for some kinds, "folder" among them.
    private static func capitalized(_ text: String) -> String {
        text.prefix(1).uppercased() + text.dropFirst()
    }

    /// "Today at 14:03", "Yesterday at 09:12", or the date for anything older.
    func date(_ date: Date?) -> String {
        date.map(dates.string(from:)) ?? "—"
    }

    func folder(_ path: String, home: String = NSHomeDirectory()) -> String {
        guard path == home || path.hasPrefix(home + "/") else { return path }
        return "~" + path.dropFirst(home.count)
    }
}

/// Beside the results while ⌘I has it on: a picture of the file, then what it is, where it is
/// and when it changed.
final class DetailsPane: NSView {
    let picture = NSImageView(
        frame: NSRect(x: (Metric.pane - Metric.thumb) / 2 + 1, y: 16, width: Metric.thumb, height: Metric.thumb))
    let name = NSTextField(labelWithString: "")
    let summary = NSTextField(labelWithString: "")
    let place = NSTextField(labelWithString: "")
    let created = NSTextField(labelWithString: "")
    let modified = NSTextField(labelWithString: "")
    private let edge = NSBox()
    private let rule = NSBox()
    private let format: DetailsFormat
    private(set) var row: Row?
    private(set) var details: Details?
    /// The file whose thumbnail Quick Look is drawing, given up as soon as another is picked.
    private(set) var waiting: URL?
    /// The file the picture shows, once its thumbnail replaced the icon.
    private(set) var thumbnailed: URL?
    private var thumbnailing: Task<Void, Never>?

    init(format: DetailsFormat = DetailsFormat()) {
        self.format = format
        super.init(frame: NSRect(x: Metric.width, y: 0, width: 1 + Metric.pane, height: Metric.details))
        edge.boxType = .separator
        edge.frame = NSRect(x: 0, y: 0, width: 1, height: Metric.details)
        edge.autoresizingMask = [.height]
        picture.imageScaling = .scaleProportionallyUpOrDown
        let text = Metric.pane - 32
        name.frame = NSRect(x: 17, y: 156, width: text, height: 17)
        name.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .semibold)
        summary.frame = NSRect(x: 17, y: 175, width: text, height: 15)
        for label in [name, summary] {
            label.alignment = .center
            label.lineBreakMode = .byTruncatingMiddle
        }
        rule.boxType = .separator
        rule.frame = NSRect(x: 17, y: 204, width: text, height: 1)
        for view in [edge, picture, name, summary, rule] { addSubview(view) }
        for (index, (title, value)) in [("Where", place), ("Created", created), ("Modified", modified)].enumerated() {
            let y = 216 + CGFloat(index) * 19
            let label = NSTextField(labelWithString: title)
            label.frame = NSRect(x: 17, y: y, width: 60, height: 15)
            label.textColor = .secondaryLabelColor
            value.frame = NSRect(x: 81, y: y, width: text - 64, height: 15)
            value.lineBreakMode = .byTruncatingMiddle
            for field in [label, value] {
                field.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
                addSubview(field)
            }
        }
        summary.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        summary.textColor = .secondaryLabelColor
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel("Details")
    }

    required init?(coder: NSCoder) { nil }

    override var isFlipped: Bool { true }

    /// Reads the file again each time, so a new list after a save shows the new size and date;
    /// the thumbnail is asked for again only when the file or its date changed.
    func show(_ row: Row?, scale: CGFloat) {
        guard let row else { return stopThumbnail() }
        let details = Details.read(row)
        guard row != self.row || details != self.details else { return }
        let other = row != self.row
        let changed = other || details.modified != self.details?.modified
        self.row = row
        self.details = details
        set(name, row.name)
        set(summary, format.summary(details))
        set(place, format.folder((row.path as NSString).deletingLastPathComponent))
        set(created, format.date(details.created))
        set(modified, format.date(details.modified))
        if other {
            picture.image = NSWorkspace.shared.icon(for: details.type)
            thumbnailed = nil
        }
        guard changed else { return }
        stopThumbnail()
        if details.found { thumbnail(of: row.url, scale: scale) }
    }

    func stopThumbnail() {
        thumbnailing?.cancel()
        thumbnailing = nil
        waiting = nil
    }

    /// The icon shows at once; the picture of the file replaces it once Quick Look has drawn it.
    private func thumbnail(of url: URL, scale: CGFloat) {
        waiting = url
        let asked = ProcessInfo.processInfo.systemUptime
        thumbnailing = Task { [weak self] in
            let image = await bestThumbnail(of: url, scale: scale)
            // Checked on the main thread, where a newer pick has already cancelled this task.
            guard let self, !Task.isCancelled else { return }
            waiting = nil
            guard let image else { return }
            picture.image = image
            thumbnailed = url
            let took = (ProcessInfo.processInfo.systemUptime - asked) * 1000
            timing.notice("thumbnail shown \(took, format: .fixed(precision: 2)) ms after the selection")
        }
    }

    private func set(_ label: NSTextField, _ text: String) {
        if label.stringValue != text { label.stringValue = text }
    }
}

/// Quick Look draws off the main thread; only the finished picture comes back. Cancelling the
/// task tells Quick Look to stop.
private nonisolated func bestThumbnail(of url: URL, scale: CGFloat) async -> NSImage? {
    nonisolated(unsafe) let request = QLThumbnailGenerator.Request(
        fileAt: url, size: CGSize(width: Metric.thumb, height: Metric.thumb), scale: scale, representationTypes: .all)
    return await withTaskCancellationHandler {
        try? await QLThumbnailGenerator.shared.generateBestRepresentation(for: request).nsImage
    } onCancel: {
        QLThumbnailGenerator.shared.cancel(request)
    }
}
