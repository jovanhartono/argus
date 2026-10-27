import CTelescope
import Foundation

struct Row: Equatable {
    let name: String
    let folder: String
    let isFolder: Bool

    /// The folder you index is listed under its full path, so its name is the path already.
    var path: String { name.hasPrefix("/") ? name : (folder as NSString).appendingPathComponent(name) }
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
    }

    /// One core per app, so the C callback can find its listener without a context pointer.
    private static var listener: ((Event) -> Void)?
    /// Rust's side is safe to free from any thread, which is where Swift runs deinit.
    nonisolated(unsafe) private let core: OpaquePointer

    /// Scans `root` as a home folder in the background. Without Full Disk Access, the folders
    /// macOS guards with a pop-up each are left out, so none appears.
    convenience init?(root: String, fullDisk: Bool, onEvent: @escaping (Event) -> Void) {
        var root = root
        self.init(onEvent: onEvent) { callback in
            root.withUTF8 { tl_core_new($0.baseAddress, $0.count, fullDisk ? 1 : 0, nil, callback) }
        }
    }

    convenience init?(standIn count: UInt64, onEvent: @escaping (Event) -> Void) {
        self.init(onEvent: onEvent) { tl_core_new_stand_in(count, nil, $0) }
    }

    /// Nil when the Rust core was built from a different header, or failed to start.
    private init?(onEvent: @escaping (Event) -> Void, start: (TlEventFn) -> OpaquePointer?) {
        guard tl_abi_version() == TL_ABI_VERSION, let core = withCallback(start) else { return nil }
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

    fileprivate static func deliver(_ event: UInt32, _ value: UInt64) {
        switch event {
        case TL_EVENT_INDEXED: listener?(.indexed(count: value))
        case TL_EVENT_READY: listener?(.ready(ticket: value))
        case TL_EVENT_FAILED: listener?(.failed(code: Int32(truncatingIfNeeded: value)))
        default: break
        }
    }
}

/// Rust calls the callback from its own worker thread. Written inside main-thread code, Swift 6
/// would stop the app on the first call, so it lives here and only hands two numbers over.
private nonisolated func withCallback(_ start: (TlEventFn) -> OpaquePointer?) -> OpaquePointer? {
    start { _, event, value in
        // A broken ignore list must show up in `log show`, and its text is gone once this returns.
        if event == TL_EVENT_NOTE, let text = UnsafePointer<CChar>(bitPattern: UInt(value)) {
            failures.error("\(String(cString: text), privacy: .public)")
            return
        }
        DispatchQueue.main.async {
            MainActor.assumeIsolated { Hatch.deliver(event, value) }
        }
    }
}
