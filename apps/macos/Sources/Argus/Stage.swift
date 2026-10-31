import AppKit
import QuickLookUI
import UniformTypeIdentifiers

/// Everything that reaches the screen or the keyboard: the search window coming forward and
/// taking the keys, Quick Look, the file, app or Finder window a result opens, and the clipboard.
/// Tests pass a stand-in, so a test run never takes the screen or the clipboard from whoever is
/// using the Mac.
@MainActor
protocol Stage: AnyObject {
    /// Argus is the active app, as after a click on the preview.
    var isActive: Bool { get }
    func isKey(_ window: NSWindow) -> Bool
    /// In front of every app, even while another one is active.
    func bringForward(_ window: NSWindow)
    func putAway(_ window: NSWindow)
    func makeKey(_ window: NSWindow)
    var previewShows: Bool { get }
    var previewIsKey: Bool { get }
    func openPreview()
    func closePreview()
    func reloadPreview()
    /// In its own app, or launched when it is an app.
    func open(_ url: URL)
    /// At that line when its app is a code editor with a way to say so; at the top otherwise.
    func open(_ url: URL, line: UInt64)
    /// In your text editor, for a file with no extension that macOS has no app for.
    func openAsText(_ url: URL)
    func showInFinder(_ url: URL)
    func copy(_ text: String)
}

@MainActor
final class Screen: Stage {
    var isActive: Bool { NSApp.isActive }

    func isKey(_ window: NSWindow) -> Bool {
        window.isKeyWindow
    }

    func bringForward(_ window: NSWindow) {
        window.orderFrontRegardless()
    }

    func putAway(_ window: NSWindow) {
        window.orderOut(nil)
    }

    func makeKey(_ window: NSWindow) {
        window.makeKey()
    }

    var previewShows: Bool {
        QLPreviewPanel.sharedPreviewPanelExists() && QLPreviewPanel.shared().isVisible
    }

    var previewIsKey: Bool {
        QLPreviewPanel.sharedPreviewPanelExists() && QLPreviewPanel.shared().isKeyWindow
    }

    func openPreview() {
        let preview = QLPreviewPanel.shared()!
        // With no change of key window to notice, it is told to look for the window that feeds it.
        preview.updateController()
        preview.orderFront(nil)
    }

    func closePreview() {
        QLPreviewPanel.shared().orderOut(nil)
    }

    func reloadPreview() {
        QLPreviewPanel.shared().reloadData()
    }

    func open(_ url: URL) {
        NSWorkspace.shared.open(url)
    }

    /// Asking macOS for the file's app takes 13 ms the first time, then about 1 ms.
    func open(_ url: URL, line: UInt64) {
        guard let app = NSWorkspace.shared.urlForApplication(toOpen: url),
            let id = Bundle(url: app)?.bundleIdentifier,
            let link = LineLink.link(app: id, path: url.path, line: line)
        else { return open(url) }
        switch link {
        case .url(let link):
            NSWorkspace.shared.open([link], withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration())
        case .xed(let arguments):
            let xed = Process()
            xed.executableURL = URL(fileURLWithPath: "/usr/bin/xed")
            xed.arguments = arguments
            do { try xed.run() } catch { open(url) }
        }
    }

    func openAsText(_ url: URL) {
        guard let editor = NSWorkspace.shared.urlForApplication(toOpen: .plainText) else { return open(url) }
        NSWorkspace.shared.open([url], withApplicationAt: editor, configuration: NSWorkspace.OpenConfiguration())
    }

    func showInFinder(_ url: URL) {
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }

    func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}
