#!/bin/zsh
# Builds build/Telescope.app for this Mac's chip.
set -euo pipefail
cd "${0:A:h}/.."

cargo build --release -p telescope-ffi
bin=$(swift build -c release --package-path apps/macos --show-bin-path)
# SwiftPM doesn't notice a rebuilt Rust library, so make it link again.
rm -f $bin/Telescope
swift build -c release --package-path apps/macos

app=build/Telescope.app
rm -rf $app
mkdir -p $app/Contents/MacOS $app/Contents/Resources
cp $bin/Telescope $app/Contents/MacOS/
cp apps/macos/Info.plist $app/Contents/
# KeyboardShortcuts looks for its bundle here, and crashes at launch without it.
cp -R $bin/*.bundle $app/Contents/Resources/

codesign --force --sign - $app
echo $app
