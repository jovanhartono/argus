#!/bin/zsh
# Builds build/Telescope.dmg: the app for Apple and Intel chips, next to a shortcut to
# Applications so installing is one drag.
set -euo pipefail
cd "${0:A:h}/.."

# The certificate v0.1.0 shipped with. macOS ties Full Disk Access to it, so a release signed
# by any other, even a new one with the same name, makes everyone grant it again.
leaf=8099307fd8f0fe98d0b0fb7774435ccdf98043ec
if [[ ${(L)$(security find-identity -p codesigning)} != *$leaf* ]]; then
  echo "The release certificate $leaf isn't in your keychains; restore it from its .p12 backup" >&2
  exit 1
fi

./scripts/build-mac.sh --universal
if [[ $(codesign -d -r- build/Telescope.app 2>&1) != *"certificate leaf = H\"$leaf\""* ]]; then
  echo "build/Telescope.app isn't signed with $leaf; remove other Telescope Self-Signed certificates" >&2
  exit 1
fi
stage=build/dmg
trap 'rm -rf $stage' EXIT
rm -rf $stage
mkdir -p $stage
cp -R build/Telescope.app $stage/
ln -s /Applications $stage/Applications
hdiutil create -volname Telescope -srcfolder $stage -format UDZO -ov build/Telescope.dmg >/dev/null
echo build/Telescope.dmg
