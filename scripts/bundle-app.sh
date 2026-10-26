#!/bin/zsh
# Builds CastMirror.app (menu-bar only, no Dock icon) into ~/Applications.
set -eu
root=${0:A:h:h}
app=${1:-$HOME/Applications/CastMirror.app}
cargo build --release --manifest-path $root/Cargo.toml
rm -rf $app && mkdir -p $app/Contents/MacOS
# The binary itself is the bundle executable (a wrapper script hides the menu-bar icon); without
# arguments inside an .app it starts in tray mode and logs to ~/Library/Logs/CastMirror.log.
cp $root/target/release/cast-mirror $app/Contents/MacOS/CastMirror
cat > $app/Contents/Info.plist <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Cast Mirror</string>
  <key>CFBundleIdentifier</key><string>de.tisth.cast-mirror</string>
  <key>CFBundleExecutable</key><string>CastMirror</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSUIElement</key><true/>
  <key>NSScreenCaptureUsageDescription</key><string>Mirrors your screen to a Cast receiver.</string>
  <key>NSLocalNetworkUsageDescription</key><string>Finds and streams to Cast receivers on your network.</string>
</dict></plist>
PLIST
# macOS ties the Screen Recording permission to the signature. An ad-hoc one changes with every build,
# so the permission is lost each time; a fixed certificate keeps it (see docs/technical.md, "Signing").
identity="CastMirror Local Signing"
if security find-identity -p codesigning | grep -q "\"$identity\""; then
  codesign --force --deep -s "$identity" $app
else
  codesign --force --deep -s - $app
fi
echo "Built $app (add it to System Settings → General → Login Items to start at login)"
