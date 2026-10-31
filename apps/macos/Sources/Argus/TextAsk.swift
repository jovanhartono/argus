struct TextAsk: Equatable {
    let query: String
    /// Only inside this folder, the chip's; everywhere when nil.
    let scope: String?
}

extension String {
    func isInside(_ folder: String) -> Bool {
        self == folder || hasPrefix(folder.hasSuffix("/") ? folder : folder + "/")
    }
}
