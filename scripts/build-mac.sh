#!/bin/zsh
# Builds build/Argus.app for this Mac's chip, or with --universal for Apple and Intel chips.
set -euo pipefail
cd "${0:A:h}/.."

flags=(-c release --package-path apps/macos)
case ${1:-} in
  --universal)
    cargo build --release -p argus-ffi --target aarch64-apple-darwin --target x86_64-apple-darwin
    # Where Package.swift looks. Every plain build puts this Mac's own library back.
    lipo -create -output target/release/libargus_ffi.a \
      target/{aarch64,x86_64}-apple-darwin/release/libargus_ffi.a
    flags+=(--arch arm64 --arch x86_64)
    ;;
  "") cargo build --release -p argus-ffi ;;
  *) echo "usage: $0 [--universal]" >&2; exit 1 ;;
esac
bin=$(swift build $flags --show-bin-path)
# SwiftPM doesn't notice a rebuilt Rust library, so make it link again.
rm -f $bin/Argus
swift build $flags

app=build/Argus.app
rm -rf $app
mkdir -p $app/Contents/MacOS $app/Contents/Resources
cp $bin/Argus $app/Contents/MacOS/
# Drops the names only a debugger needs: about a quarter of the app's size. The linker's own
# signature goes first; the real one is added below.
codesign --remove-signature $app/Contents/MacOS/Argus
strip -x $app/Contents/MacOS/Argus
cp apps/macos/Info.plist $app/Contents/
# KeyboardShortcuts looks for its bundle here, and crashes at launch without it.
cp -R $bin/*.bundle $app/Contents/Resources/

# By fingerprint: codesign won't take a certificate nobody vouches for by its name.
identity=$(security find-identity -p codesigning | awk '/"Argus Self-Signed"/ { print $2; exit }')
if [[ -n $identity ]]; then
  codesign --force --sign $identity $app
else
  # Signed for this build only: macOS forgets Argus's permissions on the next one.
  echo "No Argus Self-Signed certificate; run scripts/make-signing-cert.sh to keep permissions" >&2
  codesign --force --sign - $app
fi
echo $app
