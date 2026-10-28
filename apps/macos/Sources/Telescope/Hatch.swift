import CTelescope
import CoreServices
import Foundation

struct Row: Equatable {
    let name: String
    let folder: String
    let isFolder: Bool

    /// The folder you index is listed under its full path, so its name is the path already.
    var path: String { name.hasPrefix("/") ? name : (folder as NSString).appendingPathComponent(name) }

    var url: URL { URL(fileURLWithPath: path, isDirectory: isFolder) }
}

/// The only file that touches Rust's pointers. Rows are copied into Swift strings and Rust's
/// copy is freed at once, so nothing outside this file can point back into Rust.
@MainActor
final class Hatch {
    enum Event: Equatable {
        case indexed(count: UInt64)
        case ready(ticket: UInt64)
        /// `code` is the scan's errno, or 0 when the engine hit a bug.
        case failed(code: Int32)
        /// A list built again after a change is in use.
        case rebuilt(count: UInt64, ms: UInt64)
    }

    /// One core per app, so the C callback can find its listener without a context pointer.
    private static var listener: ((Event) -> Void)?
    /// Counts cores; each tags its events, so one still on its way from a freed core is dropped.
    private static var generation: UInt = 0
    /// Rust's side is safe to free from any thread, which is where Swift runs deinit.
    nonisolated(unsafe) private let core: OpaquePointer
    /// Changes reach Rust from here, never from the main thread.
    private let changes = DispatchQueue(label: "io.github.jovanhartono.telescope.changes")
    nonisolated(unsafe) private var stream: FSEventStreamRef?
    nonisolated(unsafe) private var names: [String: Names] = [:]
    private var watching: [String] = []
    /// Taken before the first scan starts, so a change made during it still counts.
    private let started = FSEventsGetCurrentEventId()

    /// Scans `root` as a home folder in the background. Without Full Disk Access, the folders
    /// macOS guards with a pop-up each are left out, so none appears.
    convenience init?(root: String, fullDisk: Bool, onEvent: @escaping (Event) -> Void) {
        var root = root
        self.init(onEvent: onEvent) { tag, callback in
            root.withUTF8 { tl_core_new($0.baseAddress, $0.count, fullDisk ? 1 : 0, 0, tag, callback) }
        }
    }

    convenience init?(standIn count: UInt64, onEvent: @escaping (Event) -> Void) {
        self.init(onEvent: onEvent) { tl_core_new_stand_in(count, $0, $1) }
    }

    /// Nil when the Rust core was built from a different header, or failed to start.
    private init?(
        onEvent: @escaping (Event) -> Void, start: (UnsafeMutableRawPointer?, TlEventFn) -> OpaquePointer?
    ) {
        Hatch.generation += 1
        let tag = UnsafeMutableRawPointer(bitPattern: Hatch.generation)
        guard tl_abi_version() == TL_ABI_VERSION, let core = withCallback({ start(tag, $0) }) else { return nil }
        // Events reach the main thread after this line runs, so none are missed.
        Hatch.listener = onEvent
        self.core = core
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

    /// Returns the search's ticket at once, or 0 when the core has stopped.
    func submit(_ query: String) -> UInt64 {
        var query = query
        return query.withUTF8 { tl_submit(core, $0.baseAddress, $0.count) }
    }

    /// The rows for `ticket`, or nil once a newer search exists or they were already taken.
    func take(_ ticket: UInt64) -> [Row]? {
        var out = TlRows()
        let status = tl_take(core, ticket, &out)
        if status == TL_PANICKED { Hatch.listener?(.failed(code: 0)) }
        guard status == TL_OK else { return nil }
        return copy(out)
    }

    /// Builds the list again with the folders macOS guards, now that it lets Telescope in.
    func grantFullDisk() {
        tl_grant_full_disk(core, FSEventsGetCurrentEventId())
    }

    /// Watches the folders the newest list reads. Called after each new list, as the folders
    /// may change.
    func watch() {
        var out = TlRows()
        var since: FSEventStreamEventId = 0
        guard tl_watched(core, &out, &since) == TL_OK else { return }
        let rows = copy(out)
        watchNames(of: rows.filter { !$0.isFolder }.map(\.name))
        let folders = rows.filter(\.isFolder).map(\.name)
        guard folders != watching else { return }
        watching = folders
        if let stream {
            end(stream)
            self.stream = nil
        }
        guard !folders.isEmpty else { return }
        var context = FSEventStreamContext(
            version: 0, info: UnsafeMutableRawPointer(core), retain: nil, release: nil, copyDescription: nil)
        // Replays from before the list's rebuild was asked for, so a change it missed in a folder
        // watched only from now still counts. Changes are gathered for 0.2 s, so a burst of saves
        // arrives as one call.
        guard
            let stream = FSEventStreamCreate(
                nil, changed, &context, folders as CFArray, since == 0 ? started : since, 0.2,
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
                isFolder: $0.is_folder)
        }
    }

    fileprivate static func deliver(_ tag: UInt, _ event: UInt32, _ value: UInt64) {
        guard tag == generation else { return }
        switch event {
        case TL_EVENT_INDEXED: listener?(.indexed(count: value))
        case TL_EVENT_READY: listener?(.ready(ticket: value))
        case TL_EVENT_FAILED: listener?(.failed(code: Int32(truncatingIfNeeded: value)))
        case TL_EVENT_REBUILT: listener?(.rebuilt(count: value & 0xFFFF_FFFF, ms: value >> 32))
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

/// The changed folders as one buffer of NUL-ended paths, and whether macOS lost track of some.
nonisolated func gatherChanges(
    _ count: Int, _ paths: UnsafePointer<UnsafePointer<CChar>>, _ flags: UnsafePointer<FSEventStreamEventFlags>
) -> (bytes: [UInt8], rescan: Bool) {
    var bytes: [UInt8] = []
    var rescan = false
    for event in 0..<count {
        if flags[event] & FSEventStreamEventFlags(kFSEventStreamEventFlagHistoryDone) != 0 { continue }
        if flags[event] & rescanFlags != 0 { rescan = true }
        bytes.append(contentsOf: UnsafeRawBufferPointer(start: paths[event], count: strlen(paths[event]) + 1))
    }
    return (bytes, rescan)
}

/// Runs on `changes`.
private nonisolated func changed(
    _ stream: ConstFSEventStreamRef, _ info: UnsafeMutableRawPointer?, _ count: Int,
    _ paths: UnsafeMutableRawPointer, _ flags: UnsafePointer<FSEventStreamEventFlags>,
    _ ids: UnsafePointer<FSEventStreamEventId>
) {
    guard let info else { return }
    let (bytes, rescan) = gatherChanges(count, paths.assumingMemoryBound(to: UnsafePointer<CChar>.self), flags)
    guard rescan || !bytes.isEmpty else { return }
    tl_changed(OpaquePointer(info), bytes, bytes.count, rescan ? 1 : 0, FSEventsGetCurrentEventId())
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
    let check: @Sendable () -> Void = { _ = tl_changed(core, path, path.count, 0, FSEventsGetCurrentEventId()) }
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
