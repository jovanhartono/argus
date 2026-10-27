import CTelescope
import Foundation

struct Row: Equatable {
    let name: String
    let folder: String
    let isFolder: Bool
}

/// The only file that touches Rust's pointers. Rows are copied into Swift strings and Rust's
/// copy is freed at once, so nothing outside this file can point back into Rust.
@MainActor
final class Hatch {
    enum Event: Equatable {
        case indexed(count: UInt64)
        case ready(ticket: UInt64)
        case failed
    }

    /// One core per app, so the C callback can find its listener without a context pointer.
    private static var listener: ((Event) -> Void)?
    /// Rust's side is safe to free from any thread, which is where Swift runs deinit.
    nonisolated(unsafe) private let core: OpaquePointer

    /// Nil when the Rust core was built from a different header, or failed to start.
    init?(standIn count: UInt64, onEvent: @escaping (Event) -> Void) {
        guard tl_abi_version() == TL_ABI_VERSION, let core = startStandIn(count) else { return nil }
        // Events reach the main thread after this line runs, so none are missed.
        Hatch.listener = onEvent
        self.core = core
    }

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
        if status == TL_PANICKED { Hatch.listener?(.failed) }
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
        case TL_EVENT_FAILED: listener?(.failed)
        default: break
        }
    }
}

/// Rust calls the callback from its own worker thread. Written inside main-thread code, Swift 6
/// would stop the app on the first call, so it lives here and only hands two numbers over.
private nonisolated func startStandIn(_ count: UInt64) -> OpaquePointer? {
    tl_core_new_stand_in(count, nil) { _, event, value in
        DispatchQueue.main.async {
            MainActor.assumeIsolated { Hatch.deliver(event, value) }
        }
    }
}
