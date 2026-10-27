#!/bin/zsh
# Makes the 10-year certificate build-mac.sh signs with. macOS ties the permissions you grant
# Telescope to this certificate, so they survive rebuilds. Run once per Mac.
set -euo pipefail
name="Telescope Self-Signed"
keychain=${1:-login.keychain-db}

# The same search build-mac.sh signs with, so the two scripts never disagree.
if security find-identity -p codesigning | grep -q "\"$name\""; then
  echo "$name is already in your keychains"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf $tmp' EXIT
/usr/bin/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 -subj "/CN=$name" \
  -addext "basicConstraints=critical,CA:false" \
  -addext "keyUsage=critical,digitalSignature" \
  -addext "extendedKeyUsage=critical,codeSigning" \
  -keyout $tmp/key.pem -out $tmp/cert.pem
# The .p12 only carries the key into the keychain and is deleted on exit.
pass=$(/usr/bin/openssl rand -hex 16)
/usr/bin/openssl pkcs12 -export -inkey $tmp/key.pem -in $tmp/cert.pem -out $tmp/id.p12 -passout pass:$pass
security import $tmp/id.p12 -k $keychain -P $pass -T /usr/bin/codesign
echo "Added $name to $keychain"
echo "The next build asks to use the key once: enter your Mac password and click Always Allow."
