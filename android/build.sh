#!/bin/zsh
# Builds and signs the Android TV receiver without Gradle. Optional: ./build.sh install HOST
set -eu
cd ${0:A:h}
sdk=${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}
bt=$sdk/build-tools/34.0.0
jar=$sdk/platforms/android-34/android.jar
export JAVA_HOME=${JAVA_HOME:-/opt/homebrew/opt/openjdk@17}
export PATH=$JAVA_HOME/bin:$PATH

rm -rf build && mkdir -p build/gen build/classes build/dex
manifest=AndroidManifest.xml
if [ "${GAME:-}" = 1 ]; then
  # Declares the app a game; some TVs then switch to their game picture mode.
  sed 's|<application|<application android:appCategory="game" android:isGame="true"|' $manifest > build/AndroidManifest.xml
  manifest=build/AndroidManifest.xml
fi
$bt/aapt2 compile --dir res -o build/res.zip
$bt/aapt2 link -o build/base.apk -I $jar --manifest $manifest --java build/gen \
  --min-sdk-version 30 --target-sdk-version 34 build/res.zip
javac -source 17 -target 17 -nowarn -cp $jar -d build/classes $(find src build/gen -name '*.java')
$bt/d8 --release --min-api 30 --lib $jar --output build/dex $(find build/classes -name '*.class')
(cd build/dex && zip -q ../base.apk classes.dex)
$bt/zipalign -f 4 build/base.apk build/aligned.apk
[ -f debug.keystore ] || keytool -genkeypair -keystore debug.keystore -storepass android -keypass android \
  -alias debug -keyalg RSA -validity 10000 -dname "CN=cast-mirror debug" >/dev/null 2>&1
$bt/apksigner sign --ks debug.keystore --ks-pass pass:android --out cast-mirror-receiver.apk build/aligned.apk
echo "Built android/cast-mirror-receiver.apk"

if [ "${1:-}" = install ]; then
  host=${2:?TV address, e.g. 192.168.177.56}
  adb connect $host:5555 >/dev/null
  adb -s $host:5555 install -r cast-mirror-receiver.apk
  adb -s $host:5555 shell am start -n de.tisth.castmirror/.MainActivity
fi
