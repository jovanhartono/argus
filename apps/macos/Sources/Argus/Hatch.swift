import CArgus
import CoreServices
import Foundation

struct Row: Equatable {
    let name: String
    let folder: String
    let isFolder: Bool
    /// Named as Finder shows it, without ".app".
    var isApp = false

    /// The folder you index is listed under its full path, so its name is the path already.
    var path: String {
        if isApp { return (folder as NSString).appendingPathComponent(name + ".app") }
        return name.hasPrefix("/") ? name : (folder as NSString).appendingPathComponent(name)
    }

    var url: URL { URL(fileURLWithPath: path, isDirectory: isFolder || isApp) }

    /// /Applications/Safari.app is a link into the system's own volume; read at the link, its
    /// kind, size and picture are the link's.
    var real: URL { isApp ? url.resolvingSymlinksInPath() : url }
}

/// A line a text search found inside a file.
struct TextHit: Equatable {
    let path: String
    /// Trimmed, and cut to about 200 bytes around the match when long.
    let text: String
    /// Where the match sits in `text`, counted as AppKit counts.
    let match: NSRange
    let line: UInt64

    /// The file, as a result row, so opening, Quick Look and the details pane treat it as one.
    var file: Row {
        let url = URL(fileURLWithPath: path)
        return Row(name: url.lastPathComponent, folder: url.deletingLastPathComponent().path, isFolder: false)
    }
}

/// The lines a text search found since the last take; once `done`, nothing more comes for it.
struct TextBatch: Equatable {
    var hits: [TextHit] = []
    var done = false
    /// A bug in the engine ended the search early.
    var failed = false
    var filesSearched: UInt64 = 0
}

/// The only file that touches Rust's pointers. Rows are copied into Swift strings and Rust's
/// copy is freed at once, so nothing outside this file can point back into Rust.
@MainActor
final class Hatch {
    enum Event: Equatable {
        case indexed(count: UInt64)
        /// The list saved at the last launch is in use, in place of a first scan.
        case opened(count: UInt64)
        case ready(ticket: UInt64)
        /// `code` is the scan's errno, or 0 when the engine hit a bug.
        case failed(code: Int32)
        /// A list built again after a change is in use.
        case rebuilt(count: UInt64, ms: UInt64)
        /// An open was counted or the apps changed, so the same search may answer in another order.
        case reranked
        /// Lines found inside files, or the end of that search, wait for `takeText`.
        case lines(ticket: UInt64)
    }

    /// One core per app, so the C callback can find its listener without a context pointer.
    private static var listener: ((Event) -> Void)?
    /// Counts cores; each tags its events, so one still on its way from a freed core is dropped.
    private static var generation: UInt = 0
    /// Rust's side is safe to free from any thread, which is where Swift runs deinit.
    nonisolated(unsafe) private let core: OpaquePointer
    /// Changes and opens reach Rust from here, never from the main thread: each open is saved
    /// to disk before its call returns.
    private let changes = DispatchQueue(label: "io.github.jovanhartono.argus.changes")
    nonisolated(unsafe) private var stream: FSEventStreamRef?
    nonisolated(unsafe) private var names: [String: Names] = [:]
    private(set) var watching: [String] = []
    /// Taken before the first scan starts, so a change made during it still counts.
    private let started: FSEventStreamEventId

    /// Scans `root` as a home folder in the background. Without Full Disk Access, the folders
    /// macOS guards with a pop-up each are left out, so none appears. With `saved`, the list
    /// kept in that file at the last launch opens instead, and each new list is kept there. With
    /// `everything`, clutter and the ignore list's folders are listed too.
    convenience init?(
        root: String, fullDisk: Bool, everything: Bool = false, saved: String? = nil,
        onEvent: @escaping (Event) -> Void
    ) {
        var root = root
        // A volume FSEvents keeps no log for can't catch up after a relaunch, so nothing is saved.
        let journal = saved == nil ? nil : Hatch.journal(of: root)
        let (full, all): (UInt32, UInt32) = (fullDisk ? 1 : 0, everything ? 1 : 0)
        self.init(onEvent: onEvent) { tag, callback, now in
            root.withUTF8 { root in
                guard var file = saved, var journal else {
                    return tl_core_new(root.baseAddress, root.count, full, all, nil, tag, callback)
                }
                return file.withUTF8 { file in
                    journal.withUTF8 { journal in
                        var saved = TlSaved(
                            file: file.baseAddress, file_len: file.count, journal: journal.baseAddress,
                            journal_len: journal.count, now: now)
                        return tl_core_new(root.baseAddress, root.count, full, all, &saved, tag, callback)
                    }
                }
            }
        }
    }

    convenience init?(standIn count: UInt64, onEvent: @escaping (Event) -> Void) {
        self.init(onEvent: onEvent) { tag, callback, _ in tl_core_new_stand_in(count, tag, callback) }
    }

    /// Nil when the Rust core was built from a different header, or failed to start.
    private init?(
        onEvent: @escaping (Event) -> Void,
        start: (UnsafeMutableRawPointer?, TlEventFn, FSEventStreamEventId) -> OpaquePointer?
    ) {
        Hatch.generation += 1
        let tag = UnsafeMutableRawPointer(bitPattern: Hatch.generation)
        let started = FSEventsGetCurrentEventId()
        guard tl_abi_version() == TL_ABI_VERSION, let core = withCallback({ start(tag, $0, started) }) else {
            return nil
        }
        // Events reach the main thread after this line runs, so none are missed.
        Hatch.listener = onEvent
        self.core = core
        self.started = started
    }

    /// Where the list is kept between launches.
    nonisolated static let savedList = FileManager.default
        .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("io.github.jovanhartono.argus/index").path

    /// Where the files and apps you opened are kept, for ranking.
    nonisolated static let openedList = FileManager.default
        .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        .appendingPathComponent("io.github.jovanhartono.argus/opened").path

    /// The folders apps are installed in, as Finder's Applications folder shows them together;
    /// the same as argus_core's `top_folders`, which `tl` searches.
    nonisolated static let appFolders = ["/Applications", "/System/Applications", NSHomeDirectory() + "/Applications"]

    /// macOS's name for the FSEvents log of the volume `path` is on, or nil when it keeps none.
    nonisolated static func journal(of path: String) -> String? {
        var info = stat()
        guard stat(path, &info) == 0, let log = FSEventsCopyUUIDForDevice(info.st_dev) else {
            return nil
        }
        return CFUUIDCreateString(nil, log) as String?
    }

    /// macOS's own list of permissions opens only with Full Disk Access, and trying never shows
    /// a pop-up. Any failure counts as no access, so a doubt never brings up pop-ups.
    nonisolated static func hasFullDiskAccess(probing list: String = permissionList) -> Bool {
        let file = open(list, O_RDONLY)
        guard file >= 0 else { return false }
        close(file)
        return true
    }

    nonisolated static let permissionList =
        NSHomeDirectory() + "/Library/Application Support/com.apple.TCC/TCC.db"

    deinit {
        if let stream { end(stream) }
        for watch in names.values { watch.source.cancel() }
        // A change being handed to Rust finishes before the core goes.
        changes.sync {}
        tl_core_free(core)
    }

    /// Returns the search's ticket at once, or 0 when the core has stopped. With `folder`, a full
    /// path, only what is inside it is found.
    func submit(_ query: String, in folder: String? = nil) -> UInt64 {
        var query = query
        var folder = folder ?? ""
        return query.withUTF8 { text in
            folder.withUTF8 { tl_submit(core, text.baseAddress, text.count, $0.baseAddress, $0.count) }
        }
    }

    /// The rows for `ticket`, or nil once a newer search exists or they were already taken.
    func take(_ ticket: UInt64) -> [Row]? {
        var out = TlRows()
        let status = tl_take(core, ticket, &out)
        if status == TL_PANICKED { Hatch.listener?(.failed(code: 0)) }
        guard status == TL_OK else { return nil }
        return copy(out)
    }

    /// Searches the text inside the files in `folders`, on threads of its own, so a search by
    /// name never waits for it. Every older text search stops. Returns 0 when it can't start.
    func searchText(_ query: String, in folders: [String]) -> UInt64 {
        var query = query
        let bytes = Hatch.nulJoined(folders)
        return query.withUTF8 { tl_grep_submit(core, bytes, bytes.count, $0.baseAddress, $0.count) }
    }

    /// The lines `ticket` found since the last take, or nil when nothing waits or a newer text
    /// search took over.
    func takeText(_ ticket: UInt64) -> TextBatch? {
        var out = TlLines()
        let status = tl_grep_take(core, ticket, &out)
        if status == TL_PANICKED { Hatch.listener?(.failed(code: 0)) }
        guard status == TL_OK else { return nil }
        defer { tl_lines_free(out) }
        let bytes = { (start: UInt32, length: UInt32) in
            UnsafeBufferPointer(start: out.text + Int(start), count: Int(length))
        }
        let hits = UnsafeBufferPointer(start: out.lines, count: out.count).map { found in
            let line = bytes(found.text_start, found.text_len)
            let start = min(Int(found.match_start), line.count)
            let end = min(start + Int(found.match_len), line.count)
            let utf16 = { (part: Slice<UnsafeBufferPointer<UInt8>>) in String(decoding: part, as: UTF8.self).utf16.count }
            return TextHit(
                path: String(decoding: bytes(found.path_start, found.path_len), as: UTF8.self),
                text: String(decoding: line, as: UTF8.self),
                match: NSRange(location: utf16(line[..<start]), length: utf16(line[start..<end])),
                line: found.line)
        }
        return TextBatch(hits: hits, done: out.done, failed: out.failed, filesSearched: out.report.files_searched)
    }

    func cancelText() {
        tl_grep_cancel(core)
    }

    /// Reads the apps in `folders` before the next search, so one installed since shows too.
    func readApps(in folders: [String]) {
        let bytes = Hatch.nulJoined(folders)
        _ = tl_apps(core, bytes, bytes.count)
    }

    private static func nulJoined(_ folders: [String]) -> [UInt8] {
        var bytes: [UInt8] = []
        for folder in folders {
            bytes += folder.utf8
            bytes.append(0)
        }
        return bytes
    }

    /// Ranks what you opened often and lately higher, by the opens kept in `file`.
    func rankOpens(keptIn file: String) {
        nonisolated(unsafe) let core = core
        let file = Array(file.utf8)
        changes.async { _ = tl_frecency_open(core, file, file.count) }
    }

    /// Counts one open of `path` from the next search on, and keeps it for the next launch.
    func opened(_ path: String) {
        nonisolated(unsafe) let core = core
        let path = Array(path.utf8)
        changes.async { _ = tl_opened(core, path, path.count) }
    }

    /// Builds the list again with the folders macOS guards, now that it lets Argus in, and
    /// with `everything`, clutter too in that same list.
    func grantFullDisk(everything: Bool = false) {
        tl_grant_full_disk(core, everything ? 1 : 0, FSEventsGetCurrentEventId())
    }

    /// Lists clutter and the ignore list's folders from the next list on, or leaves them out
    /// again. True when that builds a new list, which `.rebuilt` brings.
    @discardableResult
    func indexEverything(_ on: Bool) -> Bool {
        tl_index_everything(core, on ? 1 : 0, FSEventsGetCurrentEventId()) == 1
    }

    /// Never waits for a scan or a search.
    func health() -> Health {
        var out = TlHealth()
        guard tl_health(core, &out) == TL_OK else { return Health(stopped: true) }
        let date = { (seconds: UInt64) in seconds == 0 ? nil : Date(timeIntervalSince1970: TimeInterval(seconds)) }
        return Health(
            entries: out.entries, scannedAt: date(out.scanned_at), scanMs: out.scan_ms, changedAt: date(out.changed_at),
            busy: out.busy, building: out.building, behind: out.behind, stopped: out.stopped)
    }

    /// Whether a search inside the folder at `path` can find anything. Never waits for a scan
    /// or a search.
    func holds(folder path: String) -> Bool {
        var path = path
        return path.withUTF8 { tl_holds_folder(core, $0.baseAddress, $0.count) } == 1
    }

    /// Reads the whole folder again, as after a change macOS lost track of.
    func rebuild() {
        _ = tl_changed(core, nil, 0, 1, 0)
    }

    /// Watches the folders the newest list reads. Called after each new list, as the folders
    /// may change.
    func watch() {
        watch(replaying: nil)
    }

    private func watch(replaying from: FSEventStreamEventId?) {
        var out = TlRows()
        var since: FSEventStreamEventId = 0
        guard tl_watched(core, &out, &since) == TL_OK else { return }
        let rows = copy(out)
        watchNames(of: rows.filter { !$0.isFolder }.map(\.name))
        // Replays from before the list's rebuild was asked for, so a change it missed in a folder
        // watched only from now still counts.
        watch(rows.filter(\.isFolder).map(\.name), since: from ?? (since == 0 ? started : since))
    }

    /// True when the core takes the drive; `.rebuilt` follows once it is read.
    @discardableResult
    func volumeAdded(_ path: String) -> Bool {
        var path = path
        return path.withUTF8 { tl_volume_added(core, $0.baseAddress, $0.count, FSEventsGetCurrentEventId()) } == 1
    }

    /// True when the core had the drive. Its watch ends at once, before macOS checks whether
    /// anything holds it, so Finder ejects it on the first try.
    @discardableResult
    func volumeRemoved(_ path: String) -> Bool {
        var path = path
        let held = path.withUTF8 { tl_volume_removed(core, $0.baseAddress, $0.count, FSEventsGetCurrentEventId()) } == 1
        // From where the old watch stopped, so a change at home during the swap still counts.
        if held { watch(replaying: heardUpTo) }
        return held
    }

    var heardUpTo: FSEventStreamEventId {
        stream.map { FSEventStreamGetLatestEventId($0) } ?? started
    }

    /// Changes are gathered for 0.2 s, so a burst of saves arrives as one call.
    func watch(_ folders: [String], since: FSEventStreamEventId) {
        guard folders != watching else { return }
        watching = folders
        if let stream {
            end(stream)
            self.stream = nil
        }
        guard !folders.isEmpty else { return }
        var context = FSEventStreamContext(
            version: 0, info: UnsafeMutableRawPointer(core), retain: nil, release: nil, copyDescription: nil)
        guard
            let stream = FSEventStreamCreate(
                nil, changed, &context, folders as CFArray, since, 0.2,
                FSEventStreamCreateFlags(kFSEventStreamCreateFlagNone))
        else { return }
        FSEventStreamSetDispatchQueue(stream, changes)
        FSEventStreamStart(stream)
        self.stream = stream
    }

    /// A folder deleted and made again needs a new watch under the same path.
    private func watchNames(of folders: [String]) {
        for (path, watch) in names where !folders.contains(path) {
            watch.source.cancel()
            names[path] = nil
        }
        for folder in folders {
            let file = FileIdentity(of: folder)
            guard names[folder] == nil || names[folder]?.file != file else { continue }
            names[folder]?.source.cancel()
            names[folder] = watchingNames(of: folder, core: core, on: changes).map { Names(source: $0, file: file) }
        }
    }

    private func copy(_ out: TlRows) -> [Row] {
        defer { tl_rows_free(out) }
        let text = { (start: UInt32, length: UInt32) in
            String(
                decoding: UnsafeBufferPointer(start: out.text + Int(start), count: Int(length)),
                as: UTF8.self)
        }
        return UnsafeBufferPointer(start: out.rows, count: out.count).map {
            Row(
                name: text($0.name_start, $0.name_len),
                folder: text($0.folder_start, $0.folder_len),
                isFolder: $0.is_folder, isApp: $0.is_app)
        }
    }

    fileprivate static func deliver(_ tag: UInt, _ event: UInt32, _ value: UInt64) {
        guard tag == generation else { return }
        switch event {
        case TL_EVENT_INDEXED: listener?(.indexed(count: value))
        case TL_EVENT_OPENED: listener?(.opened(count: value))
        case TL_EVENT_READY: listener?(.ready(ticket: value))
        case TL_EVENT_FAILED: listener?(.failed(code: Int32(truncatingIfNeeded: value)))
        case TL_EVENT_REBUILT: listener?(.rebuilt(count: value & 0xFFFF_FFFF, ms: value >> 32))
        case TL_EVENT_RERANKED: listener?(.reranked)
        case TL_EVENT_LINES: listener?(.lines(ticket: value))
        default: break
        }
    }
}

/// Rust calls the callback from its own worker thread. Written inside main-thread code, Swift 6
/// would stop the app on the first call, so it lives here and only hands two numbers over.
private nonisolated func withCallback(_ start: (TlEventFn) -> OpaquePointer?) -> OpaquePointer? {
    start { tag, event, value in
        // A broken ignore list must show up in `log show`, and its text is gone once this returns.
        if event == TL_EVENT_NOTE, let text = UnsafePointer<CChar>(bitPattern: UInt(value)) {
            failures.error("\(String(cString: text), privacy: .public)")
            return
        }
        let tag = UInt(bitPattern: tag)
        DispatchQueue.main.async {
            MainActor.assumeIsolated { Hatch.deliver(tag, event, value) }
        }
    }
}

/// macOS lost track of some changes: only a rebuild is sure.
private let rescanFlags = FSEventStreamEventFlags(
    kFSEventStreamEventFlagMustScanSubDirs | kFSEventStreamEventFlagUserDropped
        | kFSEventStreamEventFlagKernelDropped | kFSEventStreamEventFlagEventIdsWrapped)

/// The changed folders as one buffer of NUL-ended paths, whether macOS lost track of some, and
/// the newest event's id.
nonisolated func gatherChanges(
    _ count: Int, _ paths: UnsafePointer<UnsafePointer<CChar>>, _ flags: UnsafePointer<FSEventStreamEventFlags>,
    _ ids: UnsafePointer<FSEventStreamEventId>
) -> (bytes: [UInt8], rescan: Bool, newest: FSEventStreamEventId) {
    var bytes: [UInt8] = []
    var rescan = false
    var newest: FSEventStreamEventId = 0
    for event in 0..<count {
        if flags[event] & FSEventStreamEventFlags(kFSEventStreamEventFlagHistoryDone) != 0 { continue }
        if flags[event] & rescanFlags != 0 { rescan = true }
        newest = max(newest, ids[event])
        bytes.append(contentsOf: UnsafeRawBufferPointer(start: paths[event], count: strlen(paths[event]) + 1))
    }
    return (bytes, rescan, newest)
}

/// Runs on `changes`.
private nonisolated func changed(
    _ stream: ConstFSEventStreamRef, _ info: UnsafeMutableRawPointer?, _ count: Int,
    _ paths: UnsafeMutableRawPointer, _ flags: UnsafePointer<FSEventStreamEventFlags>,
    _ ids: UnsafePointer<FSEventStreamEventId>
) {
    guard let info else { return }
    let (bytes, rescan, newest) = gatherChanges(
        count, paths.assumingMemoryBound(to: UnsafePointer<CChar>.self), flags, ids)
    guard rescan || !bytes.isEmpty else { return }
    // The event's own id, not the id now: an event not yet delivered may sit in between, and
    // the list saved after this change must still replay it at the next launch.
    tl_changed(OpaquePointer(info), bytes, bytes.count, rescan ? 1 : 0, newest)
}

/// Tells a folder apart from one made later under the same path.
private struct FileIdentity: Equatable {
    let device: dev_t
    let file: ino_t

    init?(of path: String) {
        var info = stat()
        guard stat(path, &info) == 0 else { return nil }
        device = info.st_dev
        file = info.st_ino
    }
}

private struct Names {
    let source: DispatchSourceFileSystemObject
    let file: FileIdentity?
}

/// FSEvents reports changes inside folders, so a folder appearing in the scanned folder, or in
/// ~/Library, is only seen by watching their own names. Kept out of main-thread code, like the
/// callback above, since its handler runs on `queue`.
private nonisolated func watchingNames(
    of folder: String, core: OpaquePointer, on queue: DispatchQueue
) -> DispatchSourceFileSystemObject? {
    let file = open(folder, O_EVTONLY)
    guard file >= 0 else { return nil }
    let source = DispatchSource.makeFileSystemObjectSource(fileDescriptor: file, eventMask: .write, queue: queue)
    nonisolated(unsafe) let core = core
    let path = Array(folder.utf8) + [0]
    // Not an FSEvents event, so it has no id of its own.
    let check: @Sendable () -> Void = { _ = tl_changed(core, path, path.count, 0, 0) }
    source.setEventHandler(handler: check)
    source.setCancelHandler { close(file) }
    source.resume()
    // A name added before the watch began, such as during the first scan, has no event to tell.
    queue.async(execute: check)
    return source
}

private nonisolated func end(_ stream: FSEventStreamRef) {
    FSEventStreamStop(stream)
    FSEventStreamInvalidate(stream)
    FSEventStreamRelease(stream)
}
