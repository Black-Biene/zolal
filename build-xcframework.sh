#!/bin/sh
# Build Zolal.xcframework: the Rust engine as a static library for iOS devices and simulators.
#
#   ./build-xcframework.sh [output-dir]        (default: target/Zolal.xcframework)
#
# Needs full Xcode for the last step (`xcodebuild -create-xcframework`); Command Line Tools alone
# can cross-compile the libraries but cannot package them. Install Xcode, then:
#   sudo xcode-select -s /Applications/Xcode.app/Contents/Developer
#
# Add the resulting .xcframework to the app target, along with a Swift wrapper over zolal.h
# (or wrap both in a Swift package). The module map lets Swift `import ZolalFFI`.
set -eu

crate=zolal-ffi
lib=libzolal_ffi.a
here=$(cd "$(dirname "$0")" && pwd)
cd "$here"
out=${1:-"$here/target/Zolal.xcframework"}
device_target=aarch64-apple-ios
sim_targets="aarch64-apple-ios-sim x86_64-apple-ios"

echo "==> building $crate (release) for iOS"
for target in $device_target $sim_targets; do
    rustup target add "$target" >/dev/null 2>&1 || true
    cargo build --release --package "$crate" --target "$target"
done

# One simulator slice covering Apple silicon and Intel Macs.
sim_dir="$here/target/ios-sim-universal"
mkdir -p "$sim_dir"
# shellcheck disable=SC2086  # word splitting is how we iterate the target list
lipo -create $(for t in $sim_targets; do printf '%s ' "$here/target/$t/release/$lib"; done) \
    -output "$sim_dir/$lib"
echo "==> simulator slice: $(lipo -archs "$sim_dir/$lib")"

if ! xcodebuild -version >/dev/null 2>&1; then
    echo
    echo "Libraries are built, but packaging needs full Xcode:"
    echo "  device:    $here/target/$device_target/release/$lib"
    echo "  simulator: $sim_dir/$lib"
    echo "  headers:   $here/crates/$crate/include"
    echo
    echo "Install Xcode, run 'sudo xcode-select -s /Applications/Xcode.app/Contents/Developer',"
    echo "then run this script again."
    exit 2
fi

rm -rf "$out"
xcodebuild -create-xcframework \
    -library "$here/target/$device_target/release/$lib" -headers "$here/crates/$crate/include" \
    -library "$sim_dir/$lib" -headers "$here/crates/$crate/include" \
    -output "$out"
echo "==> $out"
