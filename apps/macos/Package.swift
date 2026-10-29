// swift-tools-version: 6.0
import Foundation
import PackageDescription

/// The Rust core as a static library; `cargo build --release -p argus-ffi` makes it, and
/// scripts/build-mac.sh runs that first.
let rustLibrary = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("../../target/release")
    .standardized.path

let package = Package(
    name: "Argus",
    platforms: [.macOS(.v13)],
    dependencies: [
        .package(url: "https://github.com/sindresorhus/KeyboardShortcuts", exact: "3.1.0"),
    ],
    targets: [
        .systemLibrary(name: "CArgus"),
        .executableTarget(
            name: "Argus",
            dependencies: ["CArgus", "KeyboardShortcuts"],
            linkerSettings: [.unsafeFlags(["-L", rustLibrary])]
        ),
        .testTarget(
            name: "ArgusTests",
            dependencies: ["Argus"],
            linkerSettings: [.unsafeFlags(["-L", rustLibrary])]
        ),
    ]
)
