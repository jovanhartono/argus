import Foundation

/// The folders whose files' text a search looks inside, chosen in the health view. None at
/// first: reading inside files costs far more than matching names, so you pick where.
@MainActor
final class TextFolders {
    private let defaults: UserDefaults?
    private static let key = "textFolders"
    private(set) var paths: [String]

    /// Kept in `defaults` between launches; without, only while the app runs.
    init(defaults: UserDefaults? = nil) {
        self.defaults = defaults
        paths = defaults?.stringArray(forKey: Self.key) ?? []
    }

    /// Each folder once, in the order added.
    func add(_ folders: [String]) {
        for folder in folders where !paths.contains(folder) { paths.append(folder) }
        defaults?.set(paths, forKey: Self.key)
    }

    func remove(at indexes: IndexSet) {
        for index in indexes.reversed() where paths.indices.contains(index) { paths.remove(at: index) }
        defaults?.set(paths, forKey: Self.key)
    }

    /// The folders a search inside `scope` reaches: `scope` itself when it lies in a chosen
    /// folder, and each chosen folder that lies in `scope`.
    func reached(within scope: String?) -> [String] {
        guard let scope else { return paths }
        return paths.compactMap { folder in
            if scope.isInside(folder) { return scope }
            return folder.isInside(scope) ? folder : nil
        }
    }
}

/// A text search: what was typed, and the folders it reads.
struct TextAsk: Equatable {
    let query: String
    let folders: [String]
}

extension String {
    fileprivate func isInside(_ folder: String) -> Bool {
        self == folder || hasPrefix(folder.hasSuffix("/") ? folder : folder + "/")
    }
}
