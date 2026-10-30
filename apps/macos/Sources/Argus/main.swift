import AppKit
import KeyboardShortcuts

extension KeyboardShortcuts.Name {
    static let toggleWindow = Self("toggleWindow", initial: .init(.space, modifiers: [.command, .shift]))
    /// The window's own hotkey plus ⌥; ⌥⌘Space is Finder's own search window.
    static let searchFinderFolder = Self(
        "searchFinderFolder", initial: .init(.space, modifiers: [.command, .shift, .option]))
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var window: SearchPanel?
    private var drives: Drives?

    func applicationDidFinishLaunching(_ note: Notification) {
        // ARGUS_STAND_IN=1000000 searches made-up names instead of your files, and 0 measures
        // the window's own memory without any list.
        let window = SearchPanel(
            standIn: ProcessInfo.processInfo.environment["ARGUS_STAND_IN"].flatMap(UInt64.init),
            saved: Hatch.savedList, opens: Hatch.openedList, apps: Hatch.appFolders,
            textFolders: TextFolders(defaults: .standard), everything: IndexEverything(defaults: .standard))
        window.warmUp()
        drives = Drives(
            letGo: window.letGoOfDrive, added: window.driveMounted, removing: window.driveUnmounting,
            gone: window.driveGone)
        KeyboardShortcuts.onKeyDown(for: .toggleWindow) { window.toggle() }
        KeyboardShortcuts.onKeyDown(for: .searchFinderFolder) { window.toggleInFinderFolder() }
        self.window = window
    }

    /// ⌘, while the health view itself is key, which the search window's chain doesn't reach.
    @objc func showHealth(_ sender: Any?) {
        window?.showHealth(sender)
    }
}

/// Never shown, but AppKit finds ⌘Q, ⌘V, ⌘A, ⌘Z and the search window's own keys through it.
@MainActor
func keyMenu() -> NSMenu {
    let file = NSMenu(title: "File")
    file.addItem(withTitle: "Show in Finder", action: #selector(SearchPanel.showInFinder(_:)), keyEquivalent: "\r")
    // Finder's own "Copy as Pathname" key; ⌘⇧C belongs to clipboard managers such as Maccy.
    file.addItem(withTitle: "Copy Path", action: #selector(SearchPanel.copyPath(_:)), keyEquivalent: "c")
        .keyEquivalentModifierMask = [.command, .option]
    file.addItem(withTitle: "Quick Look", action: #selector(SearchPanel.quickLook(_:)), keyEquivalent: "y")
    file.addItem(withTitle: "Details", action: #selector(SearchPanel.toggleDetails(_:)), keyEquivalent: "i")
    file.addItem(withTitle: "Search Inside Files", action: #selector(SearchPanel.toggleText(_:)), keyEquivalent: "t")
    let edit = NSMenu(title: "Edit")
    edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
    edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
    edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
    edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
    edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
    edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
    file.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
    let app = NSMenu(title: "Argus")
    app.addItem(withTitle: "Index Health…", action: #selector(SearchPanel.showHealth(_:)), keyEquivalent: ",")
    app.addItem(withTitle: "Quit Argus", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
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
