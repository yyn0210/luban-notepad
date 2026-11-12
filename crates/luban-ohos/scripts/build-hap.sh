#!/bin/sh
# Build the Rust cdylib for OpenHarmony and copy it into the HAP project.
#
# Requirements:
#   - OpenHarmony SDK / command-line tools with API 26
#   - OHOS_NDK_HOME set
#   - `rustup target add aarch64-unknown-linux-ohos`
#   - `ohrs` (https://github.com/ohos-rs/ohrs) on PATH
#
# Usage: scripts/build-hap.sh [--release] [extra ohrs args]
set -eu

crate_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$crate_dir"

ohrs build --arch arm64 "$@"

dist_dir="dist/arm64-v8a"
libs_dir="ohos/entry/libs/arm64-v8a"
types_dir="ohos/entry/src/main/ets/types/libluban_ohos"

mkdir -p "$libs_dir" "$types_dir"
cp "$dist_dir/libluban_ohos.so" "$dist_dir/libc++_shared.so" "$libs_dir/"
sed -e '${/^$/d;}' dist/index.d.ts > "$types_dir/Index.d.ts"

# The hvigor project is generated from templates so local bundle names and
# signing profiles stay out of version control (see ohos/.gitignore).
[ -f ohos/build-profile.json5 ] || cp ohos/build-profile.template.json5 ohos/build-profile.json5
[ -f ohos/AppScope/app.json5 ] || cp ohos/AppScope/app.template.json5 ohos/AppScope/app.json5

echo "native library and type declarations are ready; build the HAP with:"
echo "  cd ohos && hvigorw assembleHap"
