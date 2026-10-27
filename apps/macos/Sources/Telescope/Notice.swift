import Foundation

/// The one line the window shows while the list leaves out the folders macOS guards. The list
/// is built once at launch, so access switched on later reaches the results only after a relaunch.
enum Notice: Equatable {
    case needsAccess
    case needsRelaunch

    static let settings = URL(
        string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")!

    var text: String {
        switch self {
        case .needsAccess:
            "Allow Full Disk Access to search Desktop, Documents, Downloads and cloud drives."
        case .needsRelaunch:
            "Press ⌘Q and open Telescope again to search Desktop, Documents, Downloads and cloud drives."
        }
    }

    var opensSettings: Bool { self == .needsAccess }
}
