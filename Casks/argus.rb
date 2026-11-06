cask "argus" do
  version "0.4.0"
  sha256 "2d11b33467d9c1aa04a9c5c732eefb30d346fa9593dfd08a7aa27f191d383f67"

  url "https://github.com/jovanhartono/argus/releases/download/v#{version}/Argus.dmg"
  name "Argus"
  desc "Fast, light file and app launcher"
  homepage "https://github.com/jovanhartono/argus"

  depends_on macos: :ventura

  app "Argus.app"

  # Argus isn't notarized; without this macOS blocks every install and upgrade until Open Anyway.
  postflight_steps do
    run "/usr/bin/xattr", args: ["-dr", "com.apple.quarantine", "{{appdir}}/Argus.app"], must_succeed: false
  end

  uninstall quit: "io.github.jovanhartono.argus"

  zap trash: [
    "~/.config/argus",
    "~/Library/Application Support/io.github.jovanhartono.argus",
    "~/Library/Preferences/io.github.jovanhartono.argus.plist",
  ]
end
