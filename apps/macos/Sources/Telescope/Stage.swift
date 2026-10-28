import AppKit
import QuickLookUI

/// Everything that reaches the screen or the keyboard: the search window coming forward and
/// taking the keys, Quick Look, and the file, app or Finder window a result opens. Tests pass a
/// stand-in, so a test run never takes the screen from whoever is using the Mac.
@MainActor
protocol Stage: AnyObject {
    /// Telescope is the active app, as after a click on the preview.
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
    func showInFinder(_ url: URL)
    /// Asks for folders with the open panel; `done` gets none when cancelled.
    func chooseFolders(_ done: @escaping @MainActor ([URL]) -> Void)
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

    func showInFinder(_ url: URL) {
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }

    func chooseFolders(_ done: @escaping @MainActor ([URL]) -> Void) {
        let choose = NSOpenPanel()
        choose.canChooseFiles = false
        choose.canChooseDirectories = true
        choose.allowsMultipleSelection = true
        choose.prompt = "Add"
        choose.message = "Telescope will also search the text inside the files in these folders."
        // The open panel takes no typing while another app is active.
        NSApp.activate(ignoringOtherApps: true)
        choose.begin { response in
            MainActor.assumeIsolated { done(response == .OK ? choose.urls : []) }
        }
    }
}
