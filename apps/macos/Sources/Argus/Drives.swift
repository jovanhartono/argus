import AppKit
import DiskArbitration

/// Hears drives being plugged in and ejected. Which ones get listed is the core's call, so
/// every mounted volume is passed on, the startup disk and disk images included.
@MainActor
final class Drives {
    private let center: NotificationCenter
    nonisolated(unsafe) private var observers: [NSObjectProtocol] = []
    private let added: @MainActor (String) -> Void
    private let removing: @MainActor (String) -> Void
    private let isMounted: @MainActor (String) async -> Bool
    private let settle: Duration
    /// Told they will unmount, and not unmounted yet.
    private(set) var leaving: Set<String> = []
    /// How many ejects of each drive in `leaving` were heard, so only the latest one's check
    /// counts.
    private var ejects: [String: Int] = [:]
    private nonisolated let asker: Asker
    nonisolated(unsafe) private var session: DASession?
    private nonisolated static let questions = DispatchQueue(label: "io.github.jovanhartono.argus.unmounts")

    /// With `letGo`, macOS asks before each unmount and the drive is let go of on a queue of its
    /// own, so Finder's eject never waits on a busy main thread and never finds the drive in use.
    /// macOS waits about 10.7 s for each app's answer before it unmounts anyway (measured on
    /// macOS 26), so a drive still mounted `settle` after its latest eject stayed for good.
    init(
        center: NotificationCenter = NSWorkspace.shared.notificationCenter, mounted: [String] = Drives.mounted(),
        isMounted: @escaping @MainActor (String) async -> Bool = Drives.stillMounted,
        settle: Duration = .seconds(15), letGo: (@Sendable (String) -> Void)? = nil,
        added: @escaping @MainActor (String) -> Void, removing: @escaping @MainActor (String) -> Void
    ) {
        self.center = center
        self.added = added
        self.removing = removing
        self.isMounted = isMounted
        self.settle = settle
        asker = Asker(letGo: letGo ?? { _ in })
        asker.drives = self
        if letGo != nil { session = Drives.ask(asker) }
        for path in mounted { added(path) }
        observers = [
            hear(NSWorkspace.didMountNotification) { drives, path, _ in
                drives.leaving.remove(path)
                drives.ejects[path] = nil
                drives.added(path)
            },
            hear(NSWorkspace.willUnmountNotification) { drives, path, _ in drives.unmounting(path) },
            // Pulled out without an eject, so no warning came first.
            hear(NSWorkspace.didUnmountNotification) { drives, path, _ in
                drives.ejects[path] = nil
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
        guard let session else { return }
        DASessionSetDispatchQueue(session, nil)
        // A question already being answered still reaches `asker`.
        Drives.questions.sync {}
    }

    nonisolated static func mounted() -> [String] {
        FileManager.default.mountedVolumeURLs(includingResourceValuesForKeys: [], options: [])?.map(\.path) ?? []
    }

    /// Whether the drive at `path` is still mounted, asked off the main thread. The kernel holds
    /// a look-up into a volume until an unmount under way ends, so a slow unmount reads as gone
    /// once it is done, never as an eject that was refused.
    nonisolated static func stillMounted(_ path: String) async -> Bool {
        await withCheckedContinuation { done in
            DispatchQueue.global(qos: .utility).async {
                var info = statfs()
                guard statfs(path, &info) == 0 else { return done.resume(returning: false) }
                let on = withUnsafeBytes(of: &info.f_mntonname) { String(cString: $0.bindMemory(to: CChar.self).baseAddress!) }
                done.resume(returning: on == path)
            }
        }
    }

    /// As macOS asks, on its own queue, whether the drive at `path` may unmount.
    nonisolated func unmountAsked(_ path: String) {
        asker.ask(path)
    }

    /// An eject refused because a file on the drive is open, or an app said no, leaves it
    /// mounted with no further note, so it is listed again once that has had time to show. An
    /// eject tried again, as after a refused one, starts that time over.
    fileprivate func unmounting(_ path: String) {
        if leaving.insert(path).inserted { removing(path) }
        let eject = ejects[path, default: 0] + 1
        ejects[path] = eject
        Task { [weak self, settle] in
            try? await Task.sleep(for: settle)
            guard let self, ejects[path] == eject else { return }
            let mounted = await isMounted(path)
            guard ejects[path] == eject, leaving.remove(path) != nil else { return }
            ejects[path] = nil
            if mounted { added(path) }
        }
    }

    private nonisolated static func ask(_ asker: Asker) -> DASession? {
        guard let session = DASessionCreate(kCFAllocatorDefault) else { return nil }
        let context = Unmanaged.passUnretained(asker).toOpaque()
        DARegisterDiskUnmountApprovalCallback(
            session, nil,
            { disk, context in
                guard let context, let description = DADiskCopyDescription(disk) as? [CFString: Any],
                    let volume = description[kDADiskDescriptionVolumePathKey] as? URL
                else { return nil }
                Unmanaged<Asker>.fromOpaque(context).takeUnretainedValue().ask(volume.path)
                return nil
            }, context)
        DASessionSetDispatchQueue(session, questions)
        return session
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

/// What macOS's question before an unmount reaches. Outlives no `Drives`: its session is
/// stopped and drained first.
private final class Asker: @unchecked Sendable {
    private let letGo: @Sendable (String) -> Void
    /// Set once, then read only on the main thread.
    weak var drives: Drives?

    init(letGo: @escaping @Sendable (String) -> Void) {
        self.letGo = letGo
    }

    func ask(_ path: String) {
        letGo(path)
        DispatchQueue.main.async { MainActor.assumeIsolated { self.drives?.unmounting(path) } }
    }
}
