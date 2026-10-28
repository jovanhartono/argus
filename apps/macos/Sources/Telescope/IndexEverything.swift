import Foundation

/// The health view's "Index everything" switch: off at first, as the whole home folder,
/// Library and clutter included, is about 2.4 million entries instead of 48,000.
@MainActor
final class IndexEverything {
    private let defaults: UserDefaults?
    private static let key = "indexEverything"

    /// Kept in `defaults` between launches; without, only while the app runs.
    var isOn: Bool {
        didSet { defaults?.set(isOn, forKey: Self.key) }
    }

    init(defaults: UserDefaults? = nil) {
        self.defaults = defaults
        isOn = defaults?.bool(forKey: Self.key) ?? false
    }
}
