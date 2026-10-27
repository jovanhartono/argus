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
./scripts/make-signing-cert.sh               # once per Mac, so permissions survive rebuilds
./scripts/build-mac.sh                       # builds build/Telescope.app
open build/Telescope.app                     # then press ⌘⇧Space
swift test --package-path apps/macos
rustup target add x86_64-apple-darwin        # once, before the first .dmg
./scripts/make-dmg.sh                        # builds build/Telescope.dmg for Apple and Intel chips
```

## Install from the .dmg

Telescope isn't notarized by Apple, so macOS asks you once before it opens:

1. Open `Telescope.dmg` and drag Telescope to Applications.
2. Open Telescope. macOS says it can't check it for malware; click Done.
3. Open System Settings → Privacy & Security, scroll down and click Open Anyway.
4. Click Open Anyway again and enter your Mac password.

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
