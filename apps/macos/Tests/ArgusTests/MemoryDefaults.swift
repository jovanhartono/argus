import Foundation
import KeyboardShortcuts

extension UserDefaults {
    /// KeyboardShortcuts saves each hotkey's starting keys the first time it finds none, into the
    /// test runner's preferences file that every project's tests share (#76). Found here first,
    /// in memory only, nothing is saved.
    static func keepHotkeysInMemory() {
        let starting: [String: KeyboardShortcuts.Shortcut] = [
            "toggleWindow": .init(.space, modifiers: [.command, .shift]),
            "searchFinderFolder": .init(.space, modifiers: [.command, .shift, .option]),
        ]
        var values: [String: Any] = [:]
        for (name, keys) in starting {
            values["KeyboardShortcuts_" + name] = String(decoding: try! JSONEncoder().encode(keys), as: UTF8.self)
        }
        standard.register(defaults: values)
    }
}

/// Settings kept only in memory. A real throwaway suite leaves an empty .plist in
/// ~/Library/Preferences after every run, even once its domain is removed (#76).
final class MemoryDefaults: UserDefaults, @unchecked Sendable {
    private var values: [String: Any] = [:]

    convenience init() {
        self.init(suiteName: "argus-tests-memory")!
    }

    override func object(forKey key: String) -> Any? { values[key] }
    override func set(_ value: Any?, forKey key: String) { values[key] = value }
    override func removeObject(forKey key: String) { values[key] = nil }
}
