import AppKit

@testable import Telescope

/// Keeps the search window and Quick Look off the screen and remembers what was asked of them.
@MainActor
final class OffStage: Stage {
    var isActive = false
    /// The window that has the keys; nil when the preview or another app has them.
    var key: NSWindow?
    private(set) var forward = 0
    private(set) var previewShows = false
    var previewIsKey = false
    private(set) var opens = 0
    private(set) var closes = 0
    private(set) var reloads = 0
    private var fading = false
    private var turn = 0

    func isKey(_ window: NSWindow) -> Bool {
        key === window
    }

    func bringForward(_ window: NSWindow) {
        forward += 1
    }

    func makeKey(_ window: NSWindow) {
        key = window
        previewIsKey = false
    }

    func openPreview() {
        opens += 1
        turn += 1
        fading = false
        previewShows = true
    }

    /// As Quick Look does: it fades out over 0.2 s, and a second close meanwhile keeps it open.
    func closePreview() {
        closes += 1
        turn += 1
        guard !fading else {
            fading = false
            return
        }
        fading = true
        let mine = turn
        Task { [weak self] in
            try? await Task.sleep(for: .milliseconds(200))
            guard let self, turn == mine else { return }
            fading = false
            previewShows = false
            previewIsKey = false
        }
    }

    func reloadPreview() {
        reloads += 1
    }
}
