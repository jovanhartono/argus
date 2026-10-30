# Argus

A fast, light file and app launcher for macOS and Windows, written in Rust. "Argus" is a codename.

Status: milestone M2, the daily driver, in progress. It moves the Mac app from made-up names to real files that stay up to date as they change. Its steps are in [docs/m2-daily-driver.html](docs/m2-daily-driver.html). Most of M3, beats Spotlight, and all of M4, text search, were built early while M2's release and 7-day run are pending: [docs/m3-beats-spotlight.html](docs/m3-beats-spotlight.html) and [docs/m4-text-search.html](docs/m4-text-search.html). The plan is in [docs/research.html](docs/research.html), and the finished M1 window is in [docs/m1-window.html](docs/m1-window.html).

## Build

```sh
cargo build --release
cargo test --workspace
```

## Mac app

Needs Xcode 16 or later (Swift 6).

```sh
./scripts/make-signing-cert.sh               # once per Mac, so permissions survive rebuilds
./scripts/build-mac.sh                       # builds build/Argus.app
open build/Argus.app                     # then press ⌘⇧Space
swift test --package-path apps/macos
rustup target add x86_64-apple-darwin        # once, before the first .dmg
./scripts/make-dmg.sh                        # builds build/Argus.dmg for Apple and Intel chips
```

`swift test` never brings a window forward or takes the keys, so you can keep working while it runs; each test fails if it left a window on screen or made the test runner the active app. Two tests need the real screen and are skipped unless asked for: they open the search window and Quick Look in front of your apps and make Argus the active app for about two seconds. Run them with the screen unlocked and hands off the keyboard:

```sh
ARGUS_FOCUS_TESTS=1 swift test --package-path apps/macos --filter testOnScreen
```

## Install from the .dmg

Argus isn't notarized by Apple, so macOS asks you once before it opens:

1. Open `Argus.dmg` and drag Argus to Applications.
2. Open Argus. macOS says it can't check it for malware; click Done.
3. Open System Settings → Privacy & Security, scroll down and click Open Anyway.
4. Click Open Anyway again and enter your Mac password.
5. Press ⌘⇧Space. Until Argus has Full Disk Access, it leaves out Desktop, Documents and Downloads, and the window says so. Click Open Settings, switch Argus on under Full Disk Access, and click Quit & Reopen when macOS asks.
6. iCloud Drive, Dropbox and other cloud drives stay out of search until you press ⌘, and turn on Include iCloud Drive and cloud storage. The switch needs Full Disk Access first.

To start Argus when you log in, open System Settings → General → Login Items & Extensions, click + under Open at Login, and choose Argus in Applications.

## Update to a new release

1. Press ⌘⇧Space, then ⌘Q to quit Argus.
2. Open the new `Argus.dmg`, drag Argus to Applications and click Replace.
3. Open Argus. macOS blocks it once more, so repeat Open Anyway from the install steps.

Open Anyway comes back with every update, because macOS approves each build on its own. Full Disk Access carries over, because every release from 0.2.1 on is signed with the same certificate (checked going from 0.1.0 to 0.2.0, [#13](https://github.com/jovanhartono/argus/issues/13)). If the window ever says it needs Full Disk Access again, remove Argus from the Full Disk Access list with −, add it back with +, and quit and reopen it.

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
