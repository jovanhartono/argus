import Foundation
import ServiceManagement

/// Argus's entry in System Settings → Login Items. Tests use `MemoryLoginItems`, so the test
/// runner never lands in the real list.
protocol LoginItems: AnyObject {
    var status: SMAppService.Status { get }
    func register() throws
    func unregister() throws
}

extension SMAppService: LoginItems {}

extension LoginItems {
    static var addedKey: String { "addedToLoginItems" }

    /// Homebrew installs never open Argus, so without this the hotkey is gone after the next
    /// restart. Only once: someone who takes Argus out of Login Items keeps it out. Someone who
    /// added it by hand before 0.4 would get a second entry, so `listed` skips them. A copy
    /// macOS runs from a quarantine folder (`translocated`) would leave an entry that breaks.
    func addOnFirstLaunch(defaults: UserDefaults, translocated: Bool, listed: () -> Bool) {
        guard !defaults.bool(forKey: Self.addedKey), !translocated else { return }
        defaults.set(true, forKey: Self.addedKey)
        guard !listed() else { return }
        try? register()
    }
}

/// System Settings' Open at Login list, entries added by hand included, which `SMAppService`
/// can't see. Apple retired the call in macOS 10.11 but it still reads the list on macOS 26.
func openAtLoginHas(bundleID: String) -> Bool {
    (OldLoginItems() as any OpenAtLogin).has(bundleID: bundleID)
}

/// Called through this, the retired calls warn only where they're written.
private protocol OpenAtLogin {
    func has(bundleID: String) -> Bool
}

private struct OldLoginItems: OpenAtLogin {
    @available(macOS, deprecated: 10.11)
    func has(bundleID: String) -> Bool {
        let session = "com.apple.LSSharedFileList.SessionLoginItems" as CFString
        guard let list = LSSharedFileListCreate(nil, session, nil)?.takeRetainedValue(),
            let items = LSSharedFileListCopySnapshot(list, nil)?.takeRetainedValue() as? [LSSharedFileListItem]
        else { return false }
        // An entry on a network drive that's gone would otherwise stop launch on a mount attempt.
        let flags = LSSharedFileListResolutionFlags(
            kLSSharedFileListNoUserInteraction | kLSSharedFileListDoNotMountVolumes)
        return items.contains { item in
            let url = LSSharedFileListItemCopyResolvedURL(item, flags, nil)?.takeRetainedValue() as URL?
            return url.flatMap { Bundle(url: $0)?.bundleIdentifier } == bundleID
        }
    }
}

/// Login Items kept only while the app runs.
final class MemoryLoginItems: LoginItems {
    var status = SMAppService.Status.notRegistered
    var failing = false

    func register() throws {
        if failing { throw CocoaError(.featureUnsupported) }
        status = .enabled
    }

    func unregister() throws {
        if failing { throw CocoaError(.featureUnsupported) }
        status = .notRegistered
    }
}
