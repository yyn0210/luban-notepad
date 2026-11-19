#!/bin/sh
# Build the Rust cdylib for OpenHarmony and copy it into the HAP project.
#
# Requirements:
#   - OpenHarmony SDK / command-line tools with API 26
#   - OHOS_NDK_HOME set
#   - `rustup target add aarch64-unknown-linux-ohos`
#   - `ohrs` (https://github.com/ohos-rs/ohrs) on PATH
#
# Usage: scripts/build-hap.sh [--profile <template>] [--abi arm64|x86_64|both]
#          [--release] [extra ohrs args]
#
# --abi selects the ABI to build (default arm64, the CI target). x86_64 is
# needed for the KaihongOS desktop line; market HAPs ship both.
# --profile selects the build-profile template copied to ohos/build-profile.json5.
# It overwrites an existing generated profile; when that file differs from the
# template (e.g. the local AGC signing configuration) a backup is written to
# ohos/build-profile.json5.bak first. Without it the default HarmonyOS
# template is only copied when the file does not exist yet.
set -eu

crate_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$crate_dir"

profile_template=${OHOS_BUILD_PROFILE_TEMPLATE:-}
abi=arm64
while [ "$#" -gt 0 ]; do
    case "$1" in
        --profile)
            [ "$#" -ge 2 ] || {
                printf '%s\n' "error: --profile requires a path" >&2
                exit 2
            }
            profile_template=$2
            shift 2
            ;;
        --abi)
            [ "$#" -ge 2 ] || {
                printf '%s\n' "error: --abi requires a value" >&2
                exit 2
            }
            case "$2" in
                arm64|x86_64|both) abi=$2 ;;
                *)
                    printf '%s\n' "error: unknown --abi value: $2 (arm64|x86_64|both)" >&2
                    exit 2
                    ;;
            esac
            shift 2
            ;;
        *)
            # Anything else (e.g. --release) goes to ohrs verbatim.
            break
            ;;
    esac
done

types_dir="ohos/entry/src/main/ets/types/libluban_ohos"
mkdir -p "$types_dir"

# ohrs emits arm64 into dist/arm64-v8a and x86_64 into dist/x86_64; the HAP
# libs directories follow the ABI names hvigor packages by.
if [ "$abi" = arm64 ] || [ "$abi" = both ]; then
    ohrs build --arch arm64 "$@"
    mkdir -p ohos/entry/libs/arm64-v8a
    cp dist/arm64-v8a/libluban_ohos.so dist/arm64-v8a/libc++_shared.so \
        ohos/entry/libs/arm64-v8a/
fi
if [ "$abi" = x86_64 ] || [ "$abi" = both ]; then
    ohrs build --arch x86_64 "$@"
    mkdir -p ohos/entry/libs/x86_64
    cp dist/x86_64/libluban_ohos.so dist/x86_64/libc++_shared.so \
        ohos/entry/libs/x86_64/
fi
sed -e '${/^$/d;}' dist/index.d.ts > "$types_dir/Index.d.ts"

# The hvigor project is generated from templates so local bundle names and
# signing profiles stay out of version control (see ohos/.gitignore).
if [ -n "$profile_template" ]; then
    if [ -f ohos/build-profile.json5 ] && ! cmp -s ohos/build-profile.json5 "$profile_template"; then
        cp ohos/build-profile.json5 ohos/build-profile.json5.bak
        echo "backed up the previous ohos/build-profile.json5 to ohos/build-profile.json5.bak"
        echo "(it likely holds the local AGC signing configuration; restore it with:"
        echo "  cp ohos/build-profile.json5.bak ohos/build-profile.json5)"
    fi
    cp "$profile_template" ohos/build-profile.json5
elif [ ! -f ohos/build-profile.json5 ]; then
    cp ohos/build-profile.template.json5 ohos/build-profile.json5
fi
[ -f ohos/AppScope/app.json5 ] || cp ohos/AppScope/app.template.json5 ohos/AppScope/app.json5

echo "native library and type declarations are ready; build the HAP with:"
echo "  cd ohos && hvigorw assembleHap"
