# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

This is `AGENTS.md`; `CLAUDE.md` is a symlink to it, so every coding agent reads the same guidance.

## What Argus is

A file and app launcher for macOS (Windows planned): a Rust search engine behind a native Swift/AppKit window. The README covers install, keys and user-facing behaviour; `docs/*.html` hold the research and the per-milestone design decisions (M1–M4), worth reading before changing ranking, indexing or text search.

## Commands

```sh
# Rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings   # keeps the Windows build honest
cargo test --workspace
cargo test -p argus-core some_test_name          # one test, or a name substring

# Mac app: the Swift package links target/release/libargus_ffi.a, so build it first,
# and again after any Rust change
cargo build --release -p argus-ffi
swift test --package-path apps/macos
swift test --package-path apps/macos --filter SearchPanelTests/testSomething
ARGUS_FOCUS_TESTS=1 swift test --package-path apps/macos --filter testOnScreen  # takes the screen; opt-in only

./scripts/build-mac.sh            # build/Argus.app, signed with "Argus Self-Signed" if present
./scripts/make-dmg.sh             # release .dmg; refuses to run without the pinned release certificate

# The engine from a terminal (bin `tl`): search, bench, golden, grep, grep-bench
cargo run --release -p argus-cli -- search ~/Documents
cargo run --release -p argus-cli                    # prints every command and flag
```

The app's logs: `log show --last 5m --predicate 'subsystem == "io.github.jovanhartono.argus"'` (categories `timing`, `failure`).

CI (`.github/workflows/ci.yml`) runs fmt, clippy, `cargo test` on Windows and macOS, and the full `swift test` on the `xcode-27` runner image (Xcode 27.2). Xcode 26's Swift compiler crashes on the tests.

## Architecture

```
apps/macos (Swift, AppKit)  ──C ABI──▶  crates/argus-ffi ("the hatch", staticlib)
                                            │
                              ┌─────────────┴─────────────┐
                        crates/argus-core            crates/argus-grep
                 index, walk, update, search,     text inside files, Spotlight
                 ranking, frecency, saved index    (MDQuery), lower-priority threads
                              ▲
                        crates/argus-cli (`tl`): same engine, for measuring and debugging
```

- **argus-core** keeps every file and folder name as flat lists (`index.rs`, `siblings.rs`, `name_starts.rs`) rather than a tree, for memory. `walk.rs` does the first scan on all cores; `update.rs` patches the list in place when folders change; `store.rs` saves and reopens the index (versioned binary format) so a relaunch skips the scan; `search.rs` is the fuzzy matcher and ranking; `frecency.rs` boosts files opened often and recently; `apps.rs` lists installed apps; `volume.rs` decides which drives join; `walk.rs`'s clutter rules plus `ignore.rs` decide what is skipped. Tests use a page allocator (`pages.rs`) so memory numbers are real.
- **argus-ffi** (`crates/argus-ffi/src/lib.rs`, its module doc explains the threading) is the only boundary. The window calls in from the main thread and never waits: `tl_submit` returns a ticket, a worker calls back when rows are ready, the window copies them with `tl_take`. Folder changes, drives and saving run on their own threads; text search has its own worker (`tl_grep_submit` / `tl_grep_take`).
- **The C header is hand-written**: `crates/argus-ffi/include/argus.h` must match the Rust side, and `the_header_matches_the_rust_side` fails if it doesn't. Any change to the ABI bumps `TL_ABI_VERSION` in both; the app refuses a core with another version.
- **apps/macos**: `Hatch.swift` is the only Swift file touching Rust pointers (rows are copied into Swift values). `SearchPanel.swift` is the window and most behaviour; `Health.swift` is Settings; `Details.swift` the ⌘I pane; `Drives.swift` mount/eject handling; `Stage.swift` is everything that reaches the screen, keyboard or clipboard.

## Rules that aren't obvious from the code

- **Tests never take the screen or keyboard.** Swift tests pass `OffStage` instead of the real `Stage`, and fail if a window is left on screen or the test runner becomes the active app. Anything that needs real focus goes behind `ARGUS_FOCUS_TESTS=1`. Tests use `MemoryDefaults`, not the real user defaults.
- **Performance is a feature.** Changes on hot paths (indexing, matching, ranking, updates) come with before/after numbers from `tl bench`, `tl golden` or the tests that print timings. Ranking changes come with a test that fails without them.
- **Build with Windows in mind.** Platform calls live behind `cfg` in small modules with a Windows arm or stub; paths go through `Path`/`PathBuf`; file names are `OsStr`, not UTF-8; Unix-only test expectations are gated with `cfg!(unix)`.
- **Signing**: macOS ties Full Disk Access to the signing certificate. `make-dmg.sh` pins the release certificate's leaf hash; never regenerate it for a release (`make-signing-cert.sh` is for local builds only).
- **Homebrew**: this repo is its own tap (`Casks/argus.rb`). After publishing a release, set the cask's `version` and `sha256` (`shasum -a 256 build/Argus.dmg`) in the same change, or `brew upgrade` fails the checksum. To check a change, tap the checkout itself (`brew tap jovanhartono/argus "$PWD"`, then `brew trust jovanhartono/argus`) and run `brew style --cask jovanhartono/argus/argus`; a bare path gets checked as plain Ruby. `brew uninstall` of the cask quits a running Argus. The cask strips the quarantine mark after install, as unsigned apps in personal taps commonly do, so Homebrew users never see Open Anyway.
- `local/` is gitignored: personal measurement data and golden sets with real file names stay there.
