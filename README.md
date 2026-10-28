# Telescope

A fast, light file and app launcher for macOS and Windows, written in Rust. "Telescope" is a codename.

Status: milestone M2, the daily driver, in progress. It moves the Mac app from made-up names to real files that stay up to date as they change. Its steps are in [docs/m2-daily-driver.html](docs/m2-daily-driver.html). The plan is in [docs/research.html](docs/research.html), and the finished M1 window is in [docs/m1-window.html](docs/m1-window.html).

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

`swift test` never brings a window forward or takes the keys, so you can keep working while it runs; each test fails if it left a window on screen or made the test runner the active app. Two tests need the real screen and are skipped unless asked for: they open the search window and Quick Look in front of your apps and make Telescope the active app for about two seconds. Run them with the screen unlocked and hands off the keyboard:

```sh
TELESCOPE_FOCUS_TESTS=1 swift test --package-path apps/macos --filter testOnScreen
```

## Install from the .dmg

Telescope isn't notarized by Apple, so macOS asks you once before it opens:

1. Open `Telescope.dmg` and drag Telescope to Applications.
2. Open Telescope. macOS says it can't check it for malware; click Done.
3. Open System Settings → Privacy & Security, scroll down and click Open Anyway.
4. Click Open Anyway again and enter your Mac password.
5. Press ⌘⇧Space. Until Telescope has Full Disk Access, it leaves out Desktop, Documents, Downloads and cloud drives, and the window says so. Click Open Settings, switch Telescope on under Full Disk Access, and click Quit & Reopen when macOS asks.

To start Telescope when you log in, open System Settings → General → Login Items & Extensions, click + under Open at Login, and choose Telescope in Applications.

## Update to a new release

1. Press ⌘⇧Space, then ⌘Q to quit Telescope.
2. Open the new `Telescope.dmg`, drag Telescope to Applications and click Replace.
3. Open Telescope. If macOS asks again, repeat Open Anyway from the install steps.

Every release is signed with the same certificate, so Full Disk Access and Login Items should carry over. This hasn't been checked with a downloaded update yet ([#13](https://github.com/jovanhartono/telescope/issues/13) will record whether Open Anyway comes back and whether Full Disk Access stays on). If the window says it needs Full Disk Access again, remove Telescope from the Full Disk Access list with −, add it back with +, and quit and reopen it.

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
