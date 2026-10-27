import AppKit
import KeyboardShortcuts

extension KeyboardShortcuts.Name {
    static let toggleWindow = Self("toggleWindow", initial: .init(.space, modifiers: [.command, .shift]))
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: SearchPanel?

    func applicationDidFinishLaunching(_ note: Notification) {
        let window = SearchPanel()
        window.warmUp()
        KeyboardShortcuts.onKeyDown(for: .toggleWindow) { window.toggle() }
        self.window = window
    }
}

/// Never shown, but AppKit finds ⌘Q, ⌘V, ⌘A, ⌘Z and the search window's own keys through it.
@MainActor
private func keyMenu() -> NSMenu {
    let file = NSMenu(title: "File")
    file.addItem(withTitle: "Show in Finder", action: #selector(SearchPanel.showInFinder(_:)), keyEquivalent: "\r")
    // Finder's own "Copy as Pathname" key; ⌘⇧C belongs to clipboard managers such as Maccy.
    file.addItem(withTitle: "Copy Path", action: #selector(SearchPanel.copyPath(_:)), keyEquivalent: "c")
        .keyEquivalentModifierMask = [.command, .option]
    file.addItem(withTitle: "Quick Look", action: #selector(SearchPanel.quickLook(_:)), keyEquivalent: "y")
    let edit = NSMenu(title: "Edit")
    edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
    edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
    edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
    edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
    edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
    edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
    let app = NSMenu(title: "Telescope")
    app.addItem(withTitle: "Quit Telescope", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
    let bar = NSMenu()
    for menu in [app, file, edit] {
        bar.addItem(withTitle: menu.title, action: nil, keyEquivalent: "").submenu = menu
    }
    return bar
}

let app = NSApplication.shared
// No Dock icon and no menu bar: the hotkey is the only way in.
app.setActivationPolicy(.accessory)
app.mainMenu = keyMenu()
let delegate = AppDelegate()
app.delegate = delegate
app.run()
