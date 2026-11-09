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

# video <concat list> <mp4>
# Kept at the frames' full Retina size: a Mac shows the video at two pixels per point, so anything
# smaller is stretched and the window's text goes soft.
video() {
  local total=$(awk '$1 == "duration" { s += $2 } END { print s }' $1)
  ffmpeg -loglevel error -y -f concat -safe 0 -i $1 -t $total \
    -vf "fps=30,format=yuv420p" \
    -c:v libx264 -preset veryslow -crf 18 -tune stillimage -movflags +faststart -an $2
}
video $frames/frames.txt site/argus.mp4
video $frames/browse/frames.txt site/argus-browse.mp4
# The typo found: "desing" with design.key picked. Atlas open in the details pane.
# Lossless WebP is a quarter of the PNG's size for flat window pictures. Link previews get a PNG
# at the 1200×630 they all crop to.
webp() { cwebp -quiet -lossless -z 9 $1 -o $2; }
webp $frames/*-desing-6.png site/argus-poster.webp
ffmpeg -loglevel error -y -i $frames/*-desing-6.png \
  -vf "scale=-2:630:flags=lanczos,pad=1200:630:(ow-iw)/2:0:0xfafafa" site/argus-og.png
webp $frames/browse/*-details.png site/argus-browse-poster.webp
[[ -f $frames/settings.png ]] && webp $frames/settings.png site/settings.webp
ls -lh site/*.mp4 site/*.png site/*.webp
