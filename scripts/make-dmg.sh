#!/bin/zsh
# Builds build/Telescope.dmg: the app for Apple and Intel chips, next to a shortcut to
# Applications so installing is one drag.
set -euo pipefail
cd "${0:A:h}/.."

# An ad-hoc signed download would make everyone grant permissions again after each update.
if ! security find-identity -p codesigning | grep -q '"Telescope Self-Signed"'; then
  echo "No Telescope Self-Signed certificate; run scripts/make-signing-cert.sh first" >&2
  exit 1
fi

./scripts/build-mac.sh --universal
stage=build/dmg
trap 'rm -rf $stage' EXIT
rm -rf $stage
mkdir -p $stage
cp -R build/Telescope.app $stage/
ln -s /Applications $stage/Applications
hdiutil create -volname Telescope -srcfolder $stage -format UDZO -ov build/Telescope.dmg >/dev/null
echo build/Telescope.dmg
