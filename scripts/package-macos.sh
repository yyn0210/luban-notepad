#!/usr/bin/env bash
# Build a macOS .app bundle and zip for Luban Notepad.
#
# Usage:
#   scripts/package-macos.sh              # build release + package
#   scripts/package-macos.sh --skip-build # reuse target/release/luban-notepad
#
# Artifacts are written to dist/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

skip_build=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) skip_build=true ;;
        -h | --help)
            sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

version=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
target_dir=${CARGO_TARGET_DIR:-$root/target}
binary="$target_dir/release/luban-notepad"
dist="$root/dist"
app_name="Luban Notepad"
app="$dist/$app_name.app"
arch=$(uname -m)
zip_name="luban-notepad-${version}-macos-${arch}.zip"

if ! $skip_build; then
    echo "==> Building release binary"
    cargo build --release -p luban-app
fi

if [ ! -x "$binary" ]; then
    echo "error: missing release binary at $binary" >&2
    echo "run without --skip-build first" >&2
    exit 1
fi

echo "==> Staging $app_name.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
install -m 755 "$binary" "$app/Contents/MacOS/luban-notepad"
install -m 644 packaging/macos/luban-notepad.icns \
    "$app/Contents/Resources/luban-notepad.icns"
sed "s|__VERSION__|$version|g" packaging/macos/Info.plist \
    >"$app/Contents/Info.plist"

if command -v codesign >/dev/null 2>&1; then
    echo "==> Ad-hoc signing"
    codesign --force --deep --sign - "$app"
fi

mkdir -p "$dist"
echo "==> Creating $zip_name"
rm -f "$dist/$zip_name"
if command -v ditto >/dev/null 2>&1; then
    ditto -c -k --keepParent "$app" "$dist/$zip_name"
else
    (cd "$dist" && zip -qry "$zip_name" "$app_name.app")
fi

if command -v shasum >/dev/null 2>&1; then
    (cd "$dist" && shasum -a 256 "$zip_name" >"$zip_name.sha256")
else
    (cd "$dist" && sha256sum "$zip_name" >"$zip_name.sha256")
fi

echo
echo "Artifacts:"
ls -lh "$dist/$zip_name" "$dist/$zip_name.sha256"
