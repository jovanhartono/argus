#!/bin/zsh
# Draws the search window off screen, searching made-up files, into the site's two videos and
# their posters, plus the Settings screenshot when a test drew one. Never takes the screen or the keys.
set -euo pipefail
cd "${0:A:h}/.."

cargo build --release -p argus-ffi
frames=$(mktemp -d)
# Sizes and dates as an English page reads them ("4.2 MB", not "4,2 MB"), whatever the Mac's
# region; only the test runner's own setting, removed again on exit.
locale=$(defaults read com.apple.dt.xctest.tool AppleLocale 2>/dev/null || true)
defaults write com.apple.dt.xctest.tool AppleLocale en_US
restore() {
  rm -rf $frames
  if [[ -n $locale ]]; then defaults write com.apple.dt.xctest.tool AppleLocale $locale
  else defaults delete com.apple.dt.xctest.tool AppleLocale; fi
}
trap restore EXIT
ARGUS_SITE_FRAMES=$frames swift test --package-path apps/macos --filter Site

# video <concat list> <width> <mp4>
video() {
  local total=$(awk '$1 == "duration" { s += $2 } END { print s }' $1)
  ffmpeg -loglevel error -y -f concat -safe 0 -i $1 -t $total \
    -vf "fps=30,scale=$2:-2:flags=lanczos,format=yuv420p" \
    -c:v libx264 -preset veryslow -crf 22 -tune stillimage -movflags +faststart -an $3
}
video $frames/frames.txt 1280 site/argus.mp4
video $frames/browse/frames.txt 1600 site/argus-browse.mp4
# The typo found: "desing" with design.key picked. Atlas open in the details pane.
cp $frames/*-desing-6.png site/argus-poster.png
cp $frames/browse/*-details.png site/argus-browse-poster.png
[[ -f $frames/settings.png ]] && cp $frames/settings.png site/settings.png
ls -lh site/*.mp4 site/*.png
