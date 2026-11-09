import AppKit
import XCTest

@testable import Argus

/// Draws the site's picture of Settings (⌘,) from the real window, off screen, with made-up
/// numbers. Runs only with ARGUS_SITE_FRAMES set to a folder.
@MainActor
final class SiteSettingsTests: XCTestCase {
    private var root: URL!
    private var out: URL!
    private var stage: OffStage!

    private static let margin: CGFloat = 40
    private static let scale = 2

    override func setUp() async throws {
        _ = NSApplication.shared
        guard let folder = ProcessInfo.processInfo.environment["ARGUS_SITE_FRAMES"] else {
            throw XCTSkip("draws the site's Settings picture; set ARGUS_SITE_FRAMES to a folder")
        }
        UserDefaults.keepHotkeysInMemory()
        stage = OffStage()
        out = URL(fileURLWithPath: folder, isDirectory: true)
        try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
        // The skipped-folders row writes its path from the home folder, so a made-up one stands in:
        // the same one SiteFramesTests uses, as the app reads the home folder once per run.
        root = URL(fileURLWithPath: "/Users/Shared/argus-site-home", isDirectory: true)
        try? FileManager.default.removeItem(at: root)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        setenv("CFFIXED_USER_HOME", root.path, 1)
    }

    override func tearDown() async throws {
        unsetenv("CFFIXED_USER_HOME")
        if let root { try? FileManager.default.removeItem(at: root) }
        XCTAssertEqual(NSApp.windows.filter(\.isVisible), [], "a test put a window on the screen")
        XCTAssertFalse(NSApp.isActive, "a test made the test runner the active app")
    }

    func testSettingsForTheSite() throws {
        let now = Date()
        let health = Health(
            entries: 412_386, scannedAt: now.addingTimeInterval(-12 * 60), scanMs: 1_840,
            changedAt: now.addingTimeInterval(-3 * 60))
        let login = MemoryLoginItems()
        login.status = .enabled
        let window = HealthWindow(
            read: { health }, hasAccess: { true }, rebuild: {}, stage: stage, loginItems: login,
            ignoreList: root.appendingPathComponent(".config/argus/ignore"), quit: {},
            format: HealthFormat(locale: Locale(identifier: "en_US")),
            usableArea: { _ in NSRect(x: 0, y: 0, width: 1920, height: 1200) })
        window.panel.appearance = NSAppearance(named: .darkAqua)
        window.show()
        defer { window.hide() }

        XCTAssertEqual(window.entries.stringValue, "412,386")
        XCTAssertEqual(window.access.stringValue, "On")
        XCTAssertEqual(window.windowKeys.stringValue, "⇧⌘Space")
        XCTAssertEqual(window.finderKeys.stringValue, "⌥⇧⌘Space")
        XCTAssertEqual(window.startAtLogin.state, .on)
        try shoot(window.panel)
    }

    /// The whole window, title bar included, on the page's dark background with the rounded
    /// corners and hairline edge an off-screen drawing leaves out.
    private func shoot(_ panel: NSPanel) throws {
        let frameView = try XCTUnwrap(panel.contentView?.superview)
        frameView.layoutSubtreeIfNeeded()
        // Off screen the window is never key, so its title and close button draw greyed out; on
        // screen Settings opens key.
        let title = try XCTUnwrap(labels(in: frameView).first { $0.stringValue == panel.title })
        let keyTitle = NSTextField(labelWithString: title.stringValue)
        keyTitle.font = title.font
        keyTitle.frame = title.convert(title.bounds, to: frameView)
        frameView.addSubview(keyTitle)
        title.isHidden = true
        let close = try XCTUnwrap(panel.standardWindowButton(.closeButton))
        let closeAt = close.convert(close.bounds, to: nil)
        let size = frameView.bounds.size
        let views = try bitmap(size)
        frameView.cacheDisplay(in: frameView.bounds, to: views)

        let canvas = NSSize(width: size.width + 2 * Self.margin, height: size.height + 2 * Self.margin)
        let page = try bitmap(canvas)
        let context = try XCTUnwrap(NSGraphicsContext(bitmapImageRep: page))
        NSGraphicsContext.saveGraphicsState()
        defer { NSGraphicsContext.restoreGraphicsState() }
        NSGraphicsContext.current = context
        NSColor(srgbRed: 0x0d / 255, green: 0x0d / 255, blue: 0x0d / 255, alpha: 1).setFill()
        NSRect(origin: .zero, size: canvas).fill()

        let frame = NSRect(origin: NSPoint(x: Self.margin, y: Self.margin), size: size)
        let outline = NSBezierPath(roundedRect: frame, xRadius: 12, yRadius: 12)
        let dark = try XCTUnwrap(NSAppearance(named: .darkAqua))
        dark.performAsCurrentDrawingAppearance {
            NSGraphicsContext.saveGraphicsState()
            outline.addClip()
            NSColor.windowBackgroundColor.setFill()
            frame.fill()
            views.draw(in: frame, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
            let side: CGFloat = 14
            let light = NSRect(
                x: frame.minX + closeAt.midX - side / 2, y: frame.minY + closeAt.midY - side / 2, width: side, height: side)
            NSColor(srgbRed: 1, green: 0x5f / 255, blue: 0x57 / 255, alpha: 1).setFill()
            NSBezierPath(ovalIn: light).fill()
            NSGraphicsContext.restoreGraphicsState()
            NSColor.white.withAlphaComponent(0.14).setStroke()
            let edge = NSBezierPath(roundedRect: frame.insetBy(dx: 0.25, dy: 0.25), xRadius: 12, yRadius: 12)
            edge.lineWidth = 0.5
            edge.stroke()
        }
        context.flushGraphics()
        try XCTUnwrap(page.representation(using: .png, properties: [:]))
            .write(to: out.appendingPathComponent("settings.png"))
    }

    private func labels(in view: NSView) -> [NSTextField] {
        view.subviews.flatMap { sub in (sub as? NSTextField).map { [$0] } ?? labels(in: sub) }
    }

    private func bitmap(_ size: NSSize) throws -> NSBitmapImageRep {
        let rep = try XCTUnwrap(
            NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: Int(size.width) * Self.scale, pixelsHigh: Int(size.height) * Self.scale,
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                bytesPerRow: 0, bitsPerPixel: 0))
        rep.size = size
        return rep
    }
}
