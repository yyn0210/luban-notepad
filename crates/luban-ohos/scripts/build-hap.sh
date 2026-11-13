#!/bin/sh
# Build the Rust cdylib for OpenHarmony and copy it into the HAP project.
#
# Requirements:
#   - OpenHarmony SDK / command-line tools with API 26
#   - OHOS_NDK_HOME set
#   - `rustup target add aarch64-unknown-linux-ohos`
#   - `ohrs` (https://github.com/ohos-rs/ohrs) on PATH
#
# Usage: scripts/build-hap.sh [--profile <template>] [--release] [extra ohrs args]
#
# --profile selects the build-profile template copied to ohos/build-profile.json5.
# It overwrites an existing generated profile. Without it the default HarmonyOS
# template is only copied when the file does not exist yet.
set -eu

crate_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$crate_dir"

profile_template=${OHOS_BUILD_PROFILE_TEMPLATE:-}
if [ "${1:-}" = "--profile" ]; then
    [ "$#" -ge 2 ] || {
        printf '%s\n' "error: --profile requires a path" >&2
        exit 2
    }
    profile_template=$2
    shift 2
fi

ohrs build --arch arm64 "$@"

dist_dir="dist/arm64-v8a"
libs_dir="ohos/entry/libs/arm64-v8a"
types_dir="ohos/entry/src/main/ets/types/libluban_ohos"

mkdir -p "$libs_dir" "$types_dir"
cp "$dist_dir/libluban_ohos.so" "$dist_dir/libc++_shared.so" "$libs_dir/"
sed -e '${/^$/d;}' dist/index.d.ts > "$types_dir/Index.d.ts"

# The hvigor project is generated from templates so local bundle names and
# signing profiles stay out of version control (see ohos/.gitignore).
if [ -n "$profile_template" ]; then
    cp "$profile_template" ohos/build-profile.json5
elif [ ! -f ohos/build-profile.json5 ]; then
    cp ohos/build-profile.template.json5 ohos/build-profile.json5
fi
[ -f ohos/AppScope/app.json5 ] || cp ohos/AppScope/app.template.json5 ohos/AppScope/app.json5

echo "native library and type declarations are ready; build the HAP with:"
echo "  cd ohos && hvigorw assembleHap"
