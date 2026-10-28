import AppKit

/// Hears drives being plugged in and ejected. Which ones get listed is the core's call, so
/// every mounted volume is passed on, the startup disk and disk images included.
@MainActor
final class Drives {
    private let center: NotificationCenter
    nonisolated(unsafe) private var observers: [NSObjectProtocol] = []

    /// Passes each of `mounted` to `added` first, then every volume mounted later. `removing`
    /// hears of a volume before it unmounts, so whatever holds it can let go and Finder ejects
    /// it on the first try.
    init(
        center: NotificationCenter = NSWorkspace.shared.notificationCenter, mounted: [String] = Drives.mounted(),
        added: @escaping @MainActor (String) -> Void, removing: @escaping @MainActor (String) -> Void
    ) {
        self.center = center
        for path in mounted { added(path) }
        let hear = { (name: Notification.Name, then: @escaping @MainActor (String) -> Void) in
            center.addObserver(forName: name, object: nil, queue: .main) { note in
                guard let path = Drives.path(of: note) else { return }
                MainActor.assumeIsolated { then(path) }
            }
        }
        observers = [
            hear(NSWorkspace.didMountNotification, added), hear(NSWorkspace.willUnmountNotification, removing),
        ]
    }

    deinit {
        for observer in observers { center.removeObserver(observer) }
    }

    /// Read from the system's list of mounts, without touching any volume.
    nonisolated static func mounted() -> [String] {
        FileManager.default.mountedVolumeURLs(includingResourceValuesForKeys: [], options: [])?.map(\.path) ?? []
    }

    private nonisolated static func path(of note: Notification) -> String? {
        (note.userInfo?[NSWorkspace.volumeURLUserInfoKey] as? URL)?.path
    }
}
