import Foundation

/// The health view's "Include iCloud Drive and cloud storage" switch: off at first, so copies
/// kept in iCloud Drive, Dropbox and other cloud drives don't crowd the results.
@MainActor
final class CloudDrives {
    private let defaults: UserDefaults?
    private static let key = "includeCloudDrives"

    /// Kept in `defaults` between launches; without, only while the app runs.
    var isOn: Bool {
        didSet { defaults?.set(isOn, forKey: Self.key) }
    }

    init(defaults: UserDefaults? = nil) {
        self.defaults = defaults
        isOn = defaults?.bool(forKey: Self.key) ?? false
    }
}
