import AppKit
import XCTest

/// What the site's videos and pictures share: the page they sit on, the window's look on it,
/// and the made-up home folder their rows write paths from.
@MainActor
enum SitePage {
    /// The site's page color; a picture drawn on any other shows its edges on the page.
    private static let color = NSColor(srgbRed: 0xfa / 255, green: 0xfa / 255, blue: 0xfa / 255, alpha: 1)
    private static let scale = 2
    /// The rows write folders from the home folder, "~/Documents/Work", so a made-up one stands
    /// in. Not under /private/var, where Foundation drops the /private and the list doesn't, nor
    /// in the real home folder, where ⌘T reads only the files Spotlight has already seen. The
    /// same one for every test: the app reads the home folder once per run.
    static let home = URL(fileURLWithPath: "/Users/Shared/argus-site-home", isDirectory: true)

    /// An empty made-up home folder, standing in for the real one until `leaveHome()`.
    static func makeHome() throws -> URL {
        try? FileManager.default.removeItem(at: home)
        try FileManager.default.createDirectory(at: home, withIntermediateDirectories: true)
        setenv("CFFIXED_USER_HOME", home.path, 1)
        return home
    }

    static func leaveHome() {
        unsetenv("CFFIXED_USER_HOME")
        try? FileManager.default.removeItem(at: home)
    }

    static func bitmap(_ size: NSSize) throws -> NSBitmapImageRep {
        let rep = try XCTUnwrap(
            NSBitmapImageRep(
                bitmapDataPlanes: nil, pixelsWide: Int(size.width) * scale, pixelsHigh: Int(size.height) * scale,
                bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                bytesPerRow: 0, bitsPerPixel: 0))
        rep.size = size
        return rep
    }

    /// `views` at `frame` on a page of `canvas` size, as a window looks on the light page: its
    /// own background, rounded corners, a shadow and a hairline edge, none of which an
    /// off-screen drawing of its views has. `over` draws on top of the views, inside the corners.
    static func window(
        _ views: NSBitmapImageRep, at frame: NSRect, on canvas: NSSize, corner: CGFloat, over: () -> Void
    ) throws -> NSBitmapImageRep {
        let page = try bitmap(canvas)
        let context = try XCTUnwrap(NSGraphicsContext(bitmapImageRep: page))
        NSGraphicsContext.saveGraphicsState()
        defer { NSGraphicsContext.restoreGraphicsState() }
        NSGraphicsContext.current = context
        color.setFill()
        NSRect(origin: .zero, size: canvas).fill()

        let outline = NSBezierPath(roundedRect: frame, xRadius: corner, yRadius: corner)
        let light = try XCTUnwrap(NSAppearance(named: .aqua))
        light.performAsCurrentDrawingAppearance {
            NSGraphicsContext.saveGraphicsState()
            let shadow = NSShadow()
            shadow.shadowColor = NSColor.black.withAlphaComponent(0.16)
            shadow.shadowBlurRadius = 24
            shadow.shadowOffset = NSSize(width: 0, height: -8)
            shadow.set()
            NSColor.windowBackgroundColor.setFill()
            outline.fill()
            NSGraphicsContext.restoreGraphicsState()
            NSGraphicsContext.saveGraphicsState()
            outline.addClip()
            NSColor.windowBackgroundColor.setFill()
            frame.fill()
            views.draw(in: frame, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
            over()
            NSGraphicsContext.restoreGraphicsState()
            NSColor.black.withAlphaComponent(0.12).setStroke()
            let edge = NSBezierPath(roundedRect: frame.insetBy(dx: 0.25, dy: 0.25), xRadius: corner, yRadius: corner)
            edge.lineWidth = 0.5
            edge.stroke()
        }
        context.flushGraphics()
        return page
    }
}
