import Foundation

/// How a code editor is told to open a file at a line. Each answers a link of its own, a "URL
/// scheme" like vscode://; Xcode answers its xed command instead.
enum LineLink: Equatable {
    /// Sent to the editor itself, so another app that claims the same scheme can't take it.
    case url(URL)
    /// The arguments for /usr/bin/xed.
    case xed([String])

    /// Nil for TextEdit and any app without a known way: they open the file at its top.
    static func link(app: String, path: String, line: UInt64) -> LineLink? {
        switch app {
        case "com.microsoft.VSCode": fileLink("vscode", path, line)
        case "com.microsoft.VSCodeInsiders": fileLink("vscode-insiders", path, line)
        case "com.todesktop.230313mzl4w4u92": fileLink("cursor", path, line)
        case "dev.zed.Zed", "dev.zed.Zed-Preview": fileLink("zed", path, line)
        case "com.sublimetext.4", "com.sublimetext.3": sublime(path, line)
        case "com.apple.dt.Xcode": .xed(["--line", String(line), path])
        default: nil
        }
    }

    /// vscode://file/Users/me/a%20b.rs:12
    private static func fileLink(_ scheme: String, _ path: String, _ line: UInt64) -> LineLink? {
        var link = URLComponents()
        link.scheme = scheme
        link.host = "file"
        link.path = "\(path):\(line)"
        return link.url.map(LineLink.url)
    }

    /// subl://open?url=file:///Users/me/a%2520b.rs&line=12: the file's own link, escaped once more
    /// so a "&" or "#" in the name can't end it early.
    private static func sublime(_ path: String, _ line: UInt64) -> LineLink? {
        // ASCII only: `alphanumerics` would let é and 日 through unescaped.
        let plain = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789/:-._~")
        let own = URL(fileURLWithPath: path, isDirectory: false).absoluteString
        guard let file = own.addingPercentEncoding(withAllowedCharacters: plain) else { return nil }
        var link = URLComponents()
        link.scheme = "subl"
        link.host = "open"
        link.percentEncodedQuery = "url=\(file)&line=\(line)"
        return link.url.map(LineLink.url)
    }
}
