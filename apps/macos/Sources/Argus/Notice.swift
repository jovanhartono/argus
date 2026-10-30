import Foundation

/// The one line the window shows under the box, with a button to the page of System Settings
/// that fixes it. Without Full Disk Access it shows until access is switched on, after which
/// the next show builds the list again with the guarded folders, with no relaunch. The Finder
/// line shows only while the window the Finder hotkey opened is up.
enum Notice: Equatable {
    case needsAccess
    /// Argus's switch for Finder is off in Privacy & Security → Automation.
    case needsFinder

    var settings: URL {
        switch self {
        case .needsAccess: URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")!
        case .needsFinder: URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Automation")!
        }
    }

    var text: String {
        switch self {
        case .needsAccess: "Allow Full Disk Access to search Desktop, Documents, Downloads and external drives."
        case .needsFinder: "Allow Argus to control Finder to search the folder Finder shows."
        }
    }
}
