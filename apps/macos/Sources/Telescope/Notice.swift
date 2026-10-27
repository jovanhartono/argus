import Foundation

/// The one line the window shows while the list leaves out the folders macOS guards. Once
/// access is switched on, the next show builds the list again with them, with no relaunch.
enum Notice: Equatable {
    case needsAccess

    static let settings = URL(
        string: "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")!

    var text: String {
        "Allow Full Disk Access to search Desktop, Documents, Downloads and cloud drives."
    }
}
