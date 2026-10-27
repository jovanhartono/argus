# Telescope

A fast, light file and app launcher for macOS and Windows, written in Rust. "Telescope" is a codename.

Status: milestone M1, the Mac search window over a million made-up names. The plan is in [docs/research.html](docs/research.html), and M1's steps are in [docs/m1-window.html](docs/m1-window.html).

## Build

```sh
cargo build --release
cargo test --workspace
```

## Mac app

Needs Xcode 16 or later (Swift 6).

```sh
./scripts/build-mac.sh                       # builds build/Telescope.app
open build/Telescope.app                     # then press ⌘⇧Space
swift test --package-path apps/macos
```

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
