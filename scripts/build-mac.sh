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

# By fingerprint: codesign won't take a certificate nobody vouches for by its name.
identity=$(security find-identity -p codesigning | awk '/"Telescope Self-Signed"/ { print $2; exit }')
if [[ -n $identity ]]; then
  codesign --force --sign $identity $app
else
  # Signed for this build only: macOS forgets Telescope's permissions on the next one.
  echo "No Telescope Self-Signed certificate; run scripts/make-signing-cert.sh to keep permissions" >&2
  codesign --force --sign - $app
fi
echo $app
