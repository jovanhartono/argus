import AppKit

/// Hears drives being plugged in and ejected. Which ones get listed is the core's call, so
/// every mounted volume is passed on, the startup disk and disk images included.
@MainActor
final class Drives {
    private let center: NotificationCenter
    nonisolated(unsafe) private var observers: [NSObjectProtocol] = []
    private let added: @MainActor (String) -> Void
    private let removing: @MainActor (String) -> Void
    private let isMounted: (String) -> Bool
    private let settle: Duration
    /// Told they will unmount, and not unmounted yet.
    private(set) var leaving: Set<String> = []

    init(
        center: NotificationCenter = NSWorkspace.shared.notificationCenter, mounted: [String] = Drives.mounted(),
        isMounted: @escaping (String) -> Bool = { Drives.mounted().contains($0) }, settle: Duration = .seconds(10),
        added: @escaping @MainActor (String) -> Void, removing: @escaping @MainActor (String) -> Void
    ) {
        self.center = center
        self.added = added
        self.removing = removing
        self.isMounted = isMounted
        self.settle = settle
        for path in mounted { added(path) }
        observers = [
            hear(NSWorkspace.didMountNotification) { drives, path, _ in
                drives.leaving.remove(path)
                drives.added(path)
            },
            hear(NSWorkspace.willUnmountNotification) { drives, path, _ in drives.unmounting(path) },
            // Pulled out without an eject, so no warning came first.
            hear(NSWorkspace.didUnmountNotification) { drives, path, _ in
                if drives.leaving.remove(path) == nil { drives.removing(path) }
            },
            hear(NSWorkspace.didRenameVolumeNotification) { drives, path, old in
                guard let old else { return }
                drives.removing(old)
                drives.added(path)
            },
        ]
    }

    deinit {
        for observer in observers { center.removeObserver(observer) }
    }

    nonisolated static func mounted() -> [String] {
        FileManager.default.mountedVolumeURLs(includingResourceValuesForKeys: [], options: [])?.map(\.path) ?? []
    }

    /// An eject refused because a file on the drive is open leaves it mounted with no further
    /// note, so it is listed again once that has had time to show.
    private func unmounting(_ path: String) {
        leaving.insert(path)
        removing(path)
        Task { [weak self, settle] in
            try? await Task.sleep(for: settle)
            guard let self, leaving.remove(path) != nil, isMounted(path) else { return }
            added(path)
        }
    }

    private func hear(
        _ name: Notification.Name, then: @escaping @MainActor (Drives, String, String?) -> Void
    ) -> NSObjectProtocol {
        center.addObserver(forName: name, object: nil, queue: .main) { [weak self] note in
            guard let path = Drives.path(of: note, NSWorkspace.volumeURLUserInfoKey) else { return }
            let old = Drives.path(of: note, NSWorkspace.oldVolumeURLUserInfoKey)
            MainActor.assumeIsolated {
                guard let self else { return }
                then(self, path, old)
            }
        }
    }

    private nonisolated static func path(of note: Notification, _ key: String) -> String? {
        (note.userInfo?[key] as? URL)?.path
    }
}
