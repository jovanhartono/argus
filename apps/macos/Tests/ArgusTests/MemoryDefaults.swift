import Foundation

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
