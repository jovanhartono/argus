# Argus

A fast, light file and app launcher for macOS, written in Rust and Swift. Press ⌘⇧Space, type a few letters, and open any file, folder or app on your Mac. It forgives typos and finds text inside files too.

The Mac app is the only one so far. A Windows version is planned.

## Features

- **Instant results.** Argus keeps its own index of your files, so results show up while you type, and it stays up to date as files change.
- **Typo-tolerant search.** `desing` still finds `design.pdf`, and `vscode` finds Visual Studio Code.
- **Apps and files in one list.** Apps from `/Applications`, `/System/Applications` and `~/Applications` rank alongside your files.
- **Search inside files.** Press ⌘T to find a line of text anywhere in your home folder, then press Return to open it at that line in VS Code, Cursor, Zed, Sublime Text or Xcode.
- **Search inside a folder.** Press Tab on a folder, or ⌥⌘⇧Space to search the folder shown in the front Finder window.
- **Less clutter.** Results leave out `node_modules`, build output, caches and the insides of packages. Add your own folders to `~/.config/argus/ignore`.
- **Learns what you open.** Files you open often and recently rank higher.
- **Light.** One small native app with no Electron and no background services.

## Install

Argus runs on Apple and Intel Macs with macOS 13 or later. Install it with [Homebrew](https://brew.sh):

```sh
brew tap jovanhartono/tap
brew trust jovanhartono/tap
brew install --cask jovanhartono/tap/argus
```

Then open Argus from Applications.

Or download `Argus.dmg` from the [latest release](https://github.com/jovanhartono/argus/releases), open it and drag Argus to Applications. Argus isn't notarized by Apple, so macOS asks you once before it opens. The Homebrew install skips this step, as it removes the "downloaded from the internet" mark that triggers it.

1. Open Argus. macOS says it can't check it for malware; click Done.
2. Open System Settings → Privacy & Security, scroll down and click Open Anyway.
3. Click Open Anyway again and enter your Mac password.

However you installed it:

1. The search window opens, and from then on ⌘⇧Space brings it up. Until Argus has Full Disk Access, it leaves out Desktop, Documents, Downloads and external drives, and the window says so. Click Open System Settings, switch Argus on under Full Disk Access, and click Quit & Reopen when macOS asks.
2. iCloud Drive, Dropbox and other cloud drives stay out of search until you press ⌘, to open Settings and turn on Include iCloud Drive and cloud storage. This switch needs Full Disk Access first.

Opening Argus again from Applications while it's running also brings up the search window. Settings (⌘, in the search window) lets you change both hotkeys, opens your list of skipped folders (`~/.config/argus/ignore`; Edit… makes one with examples if you don't have one yet) and has a Quit Argus button.

The first time you open it, Argus adds itself to Login Items, so the hotkey still works after a restart, and macOS shows a Login Item Added notice once. When it starts at login, Argus stays out of sight until you press the hotkey. To stop that, turn off Open Argus when you log in in Settings, or remove Argus in System Settings → General → Login Items & Extensions; either way it stays off.

### Update to a new release

With Homebrew, run `brew upgrade --cask argus`. It quits Argus, replaces it, and you open it again.

With the `.dmg`:

1. Press ⌘⇧Space, then ⌘Q to quit Argus.
2. Open the new `Argus.dmg`, drag Argus to Applications and click Replace.
3. Open Argus. macOS blocks it once more, so repeat Open Anyway from the install steps.

With the `.dmg`, Open Anyway comes back with every update, because macOS approves each build separately. Full Disk Access carries over, because every release from 0.2.1 on is signed with the same certificate. If the window ever says it needs Full Disk Access again, remove Argus from the Full Disk Access list with −, add it back with +, then quit and reopen Argus.

## Keys

| Key | Does |
| --- | --- |
| ⌘⇧Space | Show or hide the search window |
| ⌥⌘⇧Space | Search inside the folder the front Finder window shows |
| ↑ ↓ | Move the selection |
| Return | Open the selected file, folder or app; on a line found inside a file, open it at that line in VS Code, Cursor, Zed, Sublime Text or Xcode |
| ⌘Return | Show it in Finder |
| ⌥⌘C | Copy its path |
| ⌘Y | Preview it with Quick Look |
| ⌘I | Show or hide the details pane |
| ⌘T | Search inside files too, anywhere in your home folder, with Spotlight naming the files |
| Tab | Search inside the selected folder; ⌫ in an empty box steps back out |
| → ← | Step into the selected folder in the details pane, and back out |
| Esc | Step out of the details pane, clear the box, then leave the folder, then hide the window |
| ⌘, | Settings |
| ⌘Q | Quit Argus |

Right-click a result for the same actions, or drag it into another app, such as Mail, to give that app a copy. Both hotkeys can be changed in Settings (⌘,).

## Build from source

You need [Rust](https://rustup.rs) (stable) and, for the Mac app, Xcode 27 or later.

```sh
git clone https://github.com/jovanhartono/argus.git
cd argus
cargo build --release
cargo test --workspace
```

### Mac app

```sh
./scripts/make-signing-cert.sh    # once per Mac, so macOS keeps Argus's permissions across rebuilds
./scripts/build-mac.sh            # builds build/Argus.app
open build/Argus.app              # ⌘⇧Space hides the window and brings it back
swift test --package-path apps/macos
```

To build the `.dmg` for Apple and Intel chips, run `rustup target add x86_64-apple-darwin` once, then `./scripts/make-dmg.sh`. It signs with the release certificate and stops if that certificate isn't in your keychain, so on your own Mac use `build-mac.sh` instead.

`swift test` never brings a window forward or takes the keyboard, so you can keep working while it runs. Each test fails if it leaves a window on screen or makes the test runner the active app. Two tests need the real screen, so they're skipped unless you ask for them: they open the search window and Quick Look in front of your apps and make Argus the active app for about two seconds. Run them with the screen unlocked and your hands off the keyboard:

```sh
ARGUS_FOCUS_TESTS=1 swift test --package-path apps/macos --filter testOnScreen
```

### Command line

`tl` runs the same search engine in a terminal, which is handy for trying out ranking changes and measuring speed:

```sh
cargo run --release -p argus-cli -- search ~/Documents   # scan, then search each line you type
cargo run --release -p argus-cli                         # lists every command
```

## Project layout

| Path | What's there |
| --- | --- |
| `crates/argus-core` | The search engine: the file index, typo-tolerant matching and ranking |
| `crates/argus-grep` | Search inside text and code files |
| `crates/argus-ffi` | The C functions the Mac app calls to reach the engine |
| `crates/argus-cli` | `tl`, the command-line front end |
| `apps/macos` | The Mac app, in Swift and AppKit |
| `scripts` | Build, signing and `.dmg` scripts |
| `docs` | Design notes: the research behind Argus and the plan for each milestone |

## Contributing

Bug reports and ideas are welcome in [issues](https://github.com/jovanhartono/argus/issues). For a bug, include your macOS version, your Mac's chip and the steps that lead to it.

Pull requests are welcome too. For anything bigger than a small fix, open an issue first so we can agree on the approach.

- Fork the repo and branch from `main`. Keep each pull request to one change.
- Title it in [Conventional Commits](https://www.conventionalcommits.org) style, with a scope when it touches one area, such as `fix(grep): …` or `docs(site): …`. Pull requests are squash-merged, so the title becomes the commit on `main`.
- In the description, say what changed and why, how you checked it, and which issue it closes (`Closes #123`).
- Changes to ranking should come with a test that fails without the change. Changes on a hot path, like indexing or matching, should include before and after timings.
- Changes to the C functions in `crates/argus-ffi` must update `crates/argus-ffi/include/argus.h` to match and bump `TL_ABI_VERSION` on both sides.
- CI doesn't check `site/`. Serve it locally, look at it in a browser at full and phone width, and say what you saw.
- The Vercel check fails on pull requests from forks until a maintainer approves it. You can ignore it.

Before you open a pull request, make sure these pass:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
swift test --package-path apps/macos   # if you changed the Mac app
```

## Licence

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in Argus, as defined in the Apache-2.0 licence, is dual licensed as above, without any additional terms or conditions.
