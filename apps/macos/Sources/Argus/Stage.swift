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
    /// In your text editor, for a file with no extension that macOS has no app for.
    func openAsText(_ url: URL)
    func showInFinder(_ url: URL)
    func copy(_ text: String)
    /// Asks for folders with the open panel, as a sheet on `window`; `done` gets none when cancelled.
    func chooseFolders(on window: NSWindow, _ done: @escaping @MainActor ([URL]) -> Void)
    /// Gives the keys back to the app you were in, if Argus took them for the open panel.
    func handBack()
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

    private var lent = false

    func chooseFolders(on window: NSWindow, _ done: @escaping @MainActor ([URL]) -> Void) {
        let choose = NSOpenPanel()
        choose.canChooseFiles = false
        choose.canChooseDirectories = true
        choose.allowsMultipleSelection = true
        choose.prompt = "Add"
        choose.message = "Argus will also search the text inside the files in these folders."
        // The open panel takes no typing while another app is active.
        if !NSApp.isActive {
            lent = true
            NSApp.activate(ignoringOtherApps: true)
        }
        choose.beginSheetModal(for: window) { response in
            MainActor.assumeIsolated { done(response == .OK ? choose.urls : []) }
        }
    }

    func handBack() {
        guard lent else { return }
        lent = false
        if NSApp.isActive && NSApp.keyWindow == nil { NSApp.deactivate() }
    }
}
