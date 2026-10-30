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
        guard let values = try? row.real.resourceValues(forKeys: keys) else {
            let guess =
                row.isApp ? .applicationBundle
                : row.isFolder ? UTType.folder : UTType(filenameExtension: (row.name as NSString).pathExtension)
            return Details(type: guess ?? .data, size: nil, created: nil, modified: nil, found: false)
        }
        return Details(
            type: values.contentType ?? (row.isFolder ? .folder : .data), kind: values.localizedTypeDescription,
            size: row.isFolder || row.isApp ? nil : values.fileSize.map(Int64.init),
            created: values.creationDate, modified: values.contentModificationDate, found: true)
    }
}

/// Something inside the picked folder, as the pane lists it.
struct Child: Equatable {
    let row: Row
    let details: Details
}

/// The `limit` newest things in a folder by date modified, each with its kind and size, read
/// off the main thread; nil when the folder can't be read. Hidden ones are left out, as Finder
/// does. Only `limit` names are kept while reading, so a folder of 100,000 files holds no more
/// memory than one of 50, and a cancelled task stops within one batch of about 1,000 names.
@concurrent nonisolated func newestChildren(of folder: String, limit: Int) async -> [Child]? {
    guard let rows = newestRows(in: folder, limit: limit) else { return nil }
    var children: [Child] = []
    for row in rows {
        if Task.isCancelled { return nil }
        children.append(Child(row: row, details: Details.read(row)))
    }
    return children
}

/// One getattrlistbulk call reads the name, kind of entry and date of about 1,000 entries at
/// once: 0.12 s for 100,000 files against 0.22-0.48 s for a stat per file.
private nonisolated func newestRows(in folder: String, limit: Int) -> [Row]? {
    let fd = open(folder, O_RDONLY | O_DIRECTORY)
    guard fd >= 0 else { return nil }
    defer { close(fd) }
    var ask = attrlist()
    ask.bitmapcount = u_short(ATTR_BIT_MAP_COUNT)
    ask.commonattr =
        attrgroup_t(ATTR_CMN_RETURNED_ATTRS) | attrgroup_t(ATTR_CMN_ERROR) | attrgroup_t(ATTR_CMN_NAME)
        | attrgroup_t(ATTR_CMN_OBJTYPE) | attrgroup_t(ATTR_CMN_MODTIME) | attrgroup_t(ATTR_CMN_FLAGS)
    let needed = attrgroup_t(ATTR_CMN_NAME) | attrgroup_t(ATTR_CMN_OBJTYPE) | attrgroup_t(ATTR_CMN_MODTIME)
    let size = 64 * 1024
    let buffer = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 8)
    defer { buffer.deallocate() }
    var kept: [(changed: timespec, row: Row)] = []
    var oldest = 0
    while !Task.isCancelled {
        let count = getattrlistbulk(fd, &ask, buffer, size, 0)
        guard count > 0 else { return count < 0 ? nil : sortedNewest(kept) }
        var entry = buffer
        for _ in 0..<count {
            defer { entry += Int(entry.loadUnaligned(as: UInt32.self)) }
            let returned = entry.loadUnaligned(fromByteOffset: 4, as: attribute_set_t.self).commonattr
            var field = entry + 4 + MemoryLayout<attribute_set_t>.size
            if returned & attrgroup_t(ATTR_CMN_ERROR) != 0 {
                if field.loadUnaligned(as: UInt32.self) != 0 { continue }
                field += 4
            }
            guard returned & needed == needed else { continue }
            let name = field + Int(field.loadUnaligned(as: attrreference_t.self).attr_dataoffset)
            field += MemoryLayout<attrreference_t>.size
            let type = field.loadUnaligned(as: fsobj_type_t.self)
            field += MemoryLayout<fsobj_type_t>.size
            let changed = field.loadUnaligned(as: timespec.self)
            field += MemoryLayout<timespec>.size
            let flags = returned & attrgroup_t(ATTR_CMN_FLAGS) != 0 ? field.loadUnaligned(as: UInt32.self) : 0
            if name.load(as: UInt8.self) == UInt8(ascii: ".") || flags & UInt32(UF_HIDDEN) != 0 { continue }
            if kept.count == limit {
                guard older(kept[oldest].changed, changed) else { continue }
                kept.remove(at: oldest)
            }
            let row = Row(
                name: String(cString: name.assumingMemoryBound(to: CChar.self)), folder: folder,
                isFolder: type == VDIR.rawValue)
            kept.append((changed, row))
            if kept.count == limit {
                oldest = kept.indices.min { older(kept[$0].changed, kept[$1].changed) }!
            }
        }
    }
    return nil
}

private nonisolated func older(_ a: timespec, _ b: timespec) -> Bool {
    (a.tv_sec, a.tv_nsec) < (b.tv_sec, b.tv_nsec)
}

private nonisolated func sortedNewest(_ kept: [(changed: timespec, row: Row)]) -> [Row] {
    kept.sorted { older($1.changed, $0.changed) || !older($0.changed, $1.changed) && $0.row.name < $1.row.name }
        .map(\.row)
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
/// and when it changed. Beside a folder, what it holds too, newest change first.
final class DetailsPane: NSView, NSTableViewDataSource, NSTableViewDelegate {
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
    let contents = NSTableView()
    private let contentsScroll = NSScrollView()
    private let contentsRule = NSBox()
    /// Says why the list is empty once the folder was read.
    let contentsNote = NSTextField(labelWithString: "")
    private(set) var children: [Child] = []
    /// The picked row is a folder, so the pane is taller and lists what it holds.
    private(set) var listsContents = false
    /// The folder being read, given up as soon as another row is picked.
    private(set) var reading: URL?
    private var listing: Task<Void, Never>?
    /// A double-click on something in the folder.
    var onOpen: ((Row) -> Void)?

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
        contentsRule.boxType = .separator
        contentsRule.frame = NSRect(x: 17, y: Metric.details - 4, width: text, height: 1)
        let column = NSTableColumn(identifier: ChildCell.id)
        column.resizingMask = .autoresizingMask
        contents.addTableColumn(column)
        contents.columnAutoresizingStyle = .lastColumnOnlyAutoresizingStyle
        contents.headerView = nil
        contents.rowHeight = Metric.child
        contents.style = .plain
        contents.intercellSpacing = .zero
        contents.backgroundColor = .clear
        // Typing and the arrows stay in the search box.
        contents.refusesFirstResponder = true
        contents.dataSource = self
        contents.delegate = self
        contents.target = self
        contents.doubleAction = #selector(openChild(_:))
        contents.setAccessibilityLabel("Contents")
        contentsScroll.documentView = contents
        contentsScroll.drawsBackground = false
        contentsScroll.hasVerticalScroller = true
        contentsScroll.autohidesScrollers = true
        contentsScroll.frame = NSRect(
            x: 9, y: Metric.details, width: Metric.pane - 16, height: Metric.folderDetails - Metric.details - 8)
        contentsNote.frame = NSRect(x: 17, y: Metric.details + 8, width: text, height: 15)
        contentsNote.alignment = .center
        contentsNote.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        contentsNote.textColor = .secondaryLabelColor
        for view in [contentsRule, contentsScroll, contentsNote] {
            view.isHidden = true
            addSubview(view)
        }
        setAccessibilityElement(true)
        setAccessibilityRole(.group)
        setAccessibilityLabel("Details")
    }

    required init?(coder: NSCoder) { nil }

    override var isFlipped: Bool { true }

    /// Reads the file again each time, so a new list after a save shows the new size and date,
    /// and "Today" turns into "Yesterday" overnight; the thumbnail is asked for again only when
    /// the file or its date changed.
    func show(_ row: Row?, scale: CGFloat) {
        guard let row else { return stop() }
        let details = Details.read(row)
        let other = row != self.row
        let changed = other || details.modified != self.details?.modified || details.found != self.details?.found
        self.row = row
        self.details = details
        set(name, row.name)
        set(summary, format.summary(details))
        set(place, format.folder((row.path as NSString).deletingLastPathComponent))
        set(created, format.date(details.created))
        set(modified, format.date(details.modified))
        // Finder opens an app or a package such as a Photos library instead of showing inside it.
        let lists = details.found && row.isFolder && !row.isApp && !details.type.conforms(to: .package)
        if lists != listsContents {
            listsContents = lists
            contentsRule.isHidden = !lists
            contentsScroll.isHidden = !lists
        }
        guard changed else { return }
        if other || !details.found {
            picture.image = NSWorkspace.shared.icon(for: details.type)
            thumbnailed = nil
        }
        cancelThumbnail()
        if details.found { thumbnail(of: row.real, scale: scale) }
        if other || !lists {
            stopListing()
            children = []
            contents.reloadData()
            contentsNote.isHidden = true
        }
        // A folder that keeps changing, such as Downloads mid-download, can't keep restarting a read.
        if lists, reading == nil { list(row) }
    }

    /// Given up before they landed, the thumbnail and the folder's contents are asked for again
    /// when the same row shows next.
    func stop() {
        guard waiting != nil || reading != nil else { return }
        cancelThumbnail()
        stopListing()
        row = nil
        details = nil
    }

    private func stopListing() {
        listing?.cancel()
        listing = nil
        reading = nil
    }

    /// Read in the background; another pick cancels the read, so arrowing past folders leaves
    /// no reads behind.
    private func list(_ folder: Row) {
        reading = folder.url
        let asked = ProcessInfo.processInfo.systemUptime
        let path = folder.path
        // Detached, the read starts at once instead of waiting for the arrow key's frame to be drawn.
        listing = Task.detached(priority: .userInitiated) { [weak self] in
            let found = await newestChildren(of: path, limit: Metric.children)
            await self?.listed(found, asked: asked)
        }
    }

    private func listed(_ found: [Child]?, asked: TimeInterval) {
        // Checked on the main thread, where a newer pick has already cancelled this task.
        guard !Task.isCancelled else { return }
        listing = nil
        reading = nil
        showChildren(found)
        let took = (ProcessInfo.processInfo.systemUptime - asked) * 1000
        timing.notice(
            "\(self.children.count) folder items shown \(took, format: .fixed(precision: 2)) ms after the selection")
    }

    /// Nil when the folder couldn't be read.
    private func showChildren(_ found: [Child]?) {
        children = found ?? []
        contents.reloadData()
        contents.scrollRowToVisible(0)
        let note = found == nil ? "Can’t be read" : children.isEmpty ? "Empty folder" : ""
        set(contentsNote, note)
        contentsNote.isHidden = note.isEmpty
    }

    func numberOfRows(in tableView: NSTableView) -> Int {
        children.count
    }

    func tableView(_ tableView: NSTableView, viewFor column: NSTableColumn?, row: Int) -> NSView? {
        let cell = tableView.makeView(withIdentifier: ChildCell.id, owner: nil) as? ChildCell ?? ChildCell()
        cell.name.stringValue = children[row].row.name
        cell.summary.stringValue = format.summary(children[row].details)
        return cell
    }

    @objc private func openChild(_ sender: Any?) {
        open(child: contents.clickedRow)
    }

    /// Below the last row, the double-click has nothing to open.
    func open(child index: Int) {
        guard children.indices.contains(index) else { return }
        onOpen?(children[index].row)
    }

    private func cancelThumbnail() {
        thumbnailing?.cancel()
        thumbnailing = nil
        waiting = nil
    }

    /// The icon shows at once; the picture of the file replaces it once Quick Look has drawn it.
    private func thumbnail(of url: URL, scale: CGFloat) {
        waiting = url
        let asked = ProcessInfo.processInfo.systemUptime
        thumbnailing = Task { [weak self] in
            let drawn = await quickLookPicture(of: url, side: Metric.thumb, scale: scale, .all)
            // Checked on the main thread, where a newer pick has already cancelled this task.
            guard let self, !Task.isCancelled else { return }
            waiting = nil
            guard let drawn else { return }
            picture.image = NSImage(
                cgImage: drawn, size: NSSize(width: CGFloat(drawn.width) / scale, height: CGFloat(drawn.height) / scale))
            thumbnailed = url
            let took = (ProcessInfo.processInfo.systemUptime - asked) * 1000
            timing.notice("thumbnail shown \(took, format: .fixed(precision: 2)) ms after the selection")
        }
    }

    private func set(_ label: NSTextField, _ text: String) {
        if label.stringValue != text { label.stringValue = text }
    }
}

/// Something in the picked folder: its name, with its kind and size under it.
final class ChildCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("child")

    let name = NSTextField(labelWithString: "")
    let summary = NSTextField(labelWithString: "")

    init() {
        super.init(frame: NSRect(x: 0, y: 0, width: Metric.pane - 16, height: Metric.child))
        identifier = Self.id
        name.frame = NSRect(x: 8, y: 14, width: Metric.pane - 32, height: 16)
        summary.frame = NSRect(x: 8, y: 1, width: Metric.pane - 32, height: 14)
        summary.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        summary.textColor = .secondaryLabelColor
        for label in [name, summary] {
            label.lineBreakMode = .byTruncatingMiddle
            label.autoresizingMask = [.width]
            addSubview(label)
        }
    }

    required init?(coder: NSCoder) { nil }
}

/// Quick Look draws off the main thread; only the finished picture comes back. Cancelling the
/// task tells Quick Look to stop.
nonisolated func quickLookPicture(
    of url: URL, side: CGFloat, scale: CGFloat, _ types: QLThumbnailGenerator.Request.RepresentationTypes
) async -> CGImage? {
    nonisolated(unsafe) let request = QLThumbnailGenerator.Request(
        fileAt: url, size: CGSize(width: side, height: side), scale: scale, representationTypes: types)
    return await withTaskCancellationHandler {
        try? await QLThumbnailGenerator.shared.generateBestRepresentation(for: request).cgImage
    } onCancel: {
        QLThumbnailGenerator.shared.cancel(request)
    }
}
