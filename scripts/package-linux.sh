#!/usr/bin/env bash
# Build a portable Linux tarball plus Debian and RPM packages for Luban Notepad.
#
# Usage:
#   scripts/package-linux.sh              # build release + package
#   scripts/package-linux.sh --skip-build # reuse target/release/luban-notepad
#
# Debian and RPM packages are built when dpkg-deb and rpmbuild are available.
# Artifacts are written to dist/.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"

skip_build=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) skip_build=true ;;
        -h | --help)
            sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "unknown option: $arg" >&2
            exit 2
            ;;
    esac
done

version=$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)
# RPM versions cannot contain '-'; Fedora convention is to map the separator of
# a prerelease to '~', which also sorts before the final release.
rpm_version=$(printf '%s' "$version" | tr '-' '~')
host_arch=$(uname -m)
case "$host_arch" in
    x86_64) deb_arch=amd64; rpm_arch=x86_64 ;;
    aarch64) deb_arch=arm64; rpm_arch=aarch64 ;;
    armv7l) deb_arch=armhf; rpm_arch=armv7hl ;;
    *) deb_arch=$host_arch; rpm_arch=$host_arch ;;
esac

maintainer="Luban Notepad Contributors <luban-notepad@users.noreply.github.com>"
target_dir=${CARGO_TARGET_DIR:-$root/target}
binary="$target_dir/release/luban-notepad"
dist="$root/dist"
tarball_name="luban-notepad-${version}-linux-${host_arch}"
stage="$dist/$tarball_name"

if ! $skip_build; then
    echo "==> Building release binary"
    cargo build --release -p luban-app
fi

if [ ! -x "$binary" ]; then
    echo "error: missing release binary at $binary" >&2
    echo "run without --skip-build first" >&2
    exit 1
fi

echo "==> Staging $tarball_name"
rm -rf "$stage"
mkdir -p "$stage"
install -m 755 "$binary" "$stage/luban-notepad"
install -m 644 README.md CHANGELOG.md LICENSE THIRD_PARTY_LICENSES.md "$stage/"
install -m 644 packaging/linux/luban-notepad.desktop "$stage/"
install -m 644 packaging/linux/luban-notepad.png "$stage/"

mkdir -p "$dist"
tar --sort=name --owner=0 --group=0 --numeric-owner \
    -C "$dist" -czf "$dist/$tarball_name.tar.gz" "$tarball_name"
(cd "$dist" && sha256sum "$tarball_name.tar.gz" >"$tarball_name.tar.gz.sha256")

# Lay out the files shared by both package formats under /usr.
stage_install_tree() {
    local dest=$1
    mkdir -p \
        "$dest/usr/bin" \
        "$dest/usr/share/applications" \
        "$dest/usr/share/doc/luban-notepad"
    install -m 755 "$binary" "$dest/usr/bin/luban-notepad"
    install -m 644 packaging/linux/luban-notepad.desktop \
        "$dest/usr/share/applications/luban-notepad.desktop"
    for size in 16 24 32 48 64 128 256 512 1024; do
        install -Dm644 "packaging/linux/icons/hicolor/${size}x${size}/apps/luban-notepad.png" \
            "$dest/usr/share/icons/hicolor/${size}x${size}/apps/luban-notepad.png"
    done
    install -m 644 README.md CHANGELOG.md THIRD_PARTY_LICENSES.md \
        "$dest/usr/share/doc/luban-notepad/"
    install -m 644 LICENSE "$dest/usr/share/doc/luban-notepad/copyright"
}

deb_path=""
if command -v dpkg-deb >/dev/null 2>&1; then
    echo "==> Building Debian package"
    deb_root="$dist/.deb-root"
    rm -rf "$deb_root"
    stage_install_tree "$deb_root"
    mkdir -p "$deb_root/DEBIAN"

    installed_size=$(du -sk "$deb_root" | cut -f1)

    # Ask dpkg for the shared-library dependencies of this exact binary. Fall
    # back to the libraries GPUI links on Debian/Ubuntu if that is unavailable.
    depends=$(mktemp -d)
    mkdir -p "$depends/debian"
    cat >"$depends/debian/control" <<EOF
Source: luban-notepad
Section: editors
Priority: optional
Maintainer: $maintainer
Build-Depends: debhelper-compat (= 13)

Package: luban-notepad
Architecture: any
Description: placeholder
EOF
    computed=$(cd "$depends" && dpkg-shlibdeps -O "$binary" 2>/dev/null |
        sed -n 's/^shlibs:Depends=//p' || true)
    rm -rf "$depends"
    if [ -z "$computed" ]; then
        computed="libc6, libgcc-s1, libx11-6, libxcb1, libxcb-xkb1, libxkbcommon0, libwayland-client0, libfontconfig1, libfreetype6, libbz2-1.0, libpng16-16, libbrotli1, libffi8, zlib1g, libexpat1, libuuid1, libxcursor1, libxrandr2, libxi6"
    fi

    # dpkg-shlibdeps also counts weak undefined symbols. Rust's optional
    # pidfd_spawnp/pidfd_getpid references are bound to GLIBC_2.39 when linking
    # on Ubuntu 24.04, so the package claims libc6 (>= 2.39) even though every
    # non-weak libc symbol is available since 2.35. Lower the floor to the
    # newest non-weak requirement so packages built on newer hosts stay
    # installable on older distributions.
    strong_libc=$(LC_ALL=C readelf --dyn-syms --wide "$binary" 2>/dev/null |
        awk '$7 == "UND" && $5 != "WEAK" && $8 ~ /@GLIBC_[0-9]/ {
            sub(/.*@GLIBC_/, "", $8)
            print $8
        }' | sort -Vu | tail -n 1) || strong_libc=""
    current_libc=$(printf '%s\n' "$computed" |
        sed -n 's/.*libc6 (>= \([^)]*\)).*/\1/p')
    if [ -n "$strong_libc" ] && [ -n "$current_libc" ] &&
        dpkg --compare-versions "$current_libc" gt "$strong_libc"; then
        computed=$(printf '%s\n' "$computed" |
            sed -E "s/libc6 \(>= [^)]*\)/libc6 (>= $strong_libc)/")
    fi

    cat >"$deb_root/DEBIAN/control" <<EOF
Package: luban-notepad
Version: $version
Section: editors
Priority: optional
Architecture: $deb_arch
Maintainer: $maintainer
Installed-Size: $installed_size
Depends: $computed
Homepage: https://github.com/luban-notepad/luban-notepad
Description: cross-platform tabbed text and source code editor
 Luban Notepad is a native desktop text and source code editor written in
 Rust and rendered with GPUI. It offers tabbed editing, syntax highlighting,
 find and replace across files, encoding and line-ending control, session
 restore, and a folder explorer.
EOF

    deb_path="$dist/luban-notepad_${version}_${deb_arch}.deb"
    dpkg-deb --root-owner-group --build "$deb_root" "$deb_path" >/dev/null
    rm -rf "$deb_root"
    (cd "$dist" && sha256sum "$(basename "$deb_path")" >"$(basename "$deb_path").sha256")
else
    echo "==> dpkg-deb not found; skipping Debian package"
fi

rpm_path=""
if command -v rpmbuild >/dev/null 2>&1; then
    echo "==> Building RPM package"
    rpm_top=$(mktemp -d)
    rpm_root="$rpm_top/BUILDROOT/luban-notepad-${rpm_version}-1.${rpm_arch}"
    stage_install_tree "$rpm_root"
    mkdir -p "$rpm_top/SPECS"

    # The file list is pre-staged above and passed with --buildroot, so the
    # spec only declares metadata and the files to package. RPM's ELF
    # dependency generator fills in Requires, including versioned libc
    # symbols, from the binary itself.
    cat >"$rpm_top/SPECS/luban-notepad.spec" <<EOF
Name: luban-notepad
Version: $rpm_version
Release: 1
Summary: Cross-platform tabbed text and source code editor
License: MulanPSL-2.0
URL: https://github.com/luban-notepad/luban-notepad
BuildArch: $rpm_arch

%description
Luban Notepad is a native desktop text and source code editor written in
Rust and rendered with GPUI. It offers tabbed editing, syntax highlighting,
find and replace across files, encoding and line-ending control, session
restore, and a folder explorer.

%files
%defattr(-,root,root,-)
/usr/bin/luban-notepad
/usr/share/applications/luban-notepad.desktop
/usr/share/icons/hicolor/*/apps/luban-notepad.png
/usr/share/doc/luban-notepad
EOF

    rpmbuild -bb \
        --define "_topdir $rpm_top" \
        --define "debug_package %{nil}" \
        --define "_binary_payload w9.gzdio" \
        --buildroot "$rpm_root" \
        "$rpm_top/SPECS/luban-notepad.spec" >/dev/null

    built_rpm=$(find "$rpm_top/RPMS" -name '*.rpm' -print -quit)
    if [ -z "$built_rpm" ]; then
        echo "error: rpmbuild did not produce a package" >&2
        exit 1
    fi
    rpm_path="$dist/luban-notepad-${rpm_version}-1.${rpm_arch}.rpm"
    cp "$built_rpm" "$rpm_path"
    (cd "$dist" && sha256sum "$(basename "$rpm_path")" >"$(basename "$rpm_path").sha256")
    rm -rf "$rpm_top"
else
    echo "==> rpmbuild not found; skipping RPM package"
fi

echo
echo "Artifacts:"
ls -lh "$dist/$tarball_name.tar.gz" "$dist/$tarball_name.tar.gz.sha256"
if [ -n "$deb_path" ]; then
    ls -lh "$deb_path" "$deb_path.sha256"
fi
if [ -n "$rpm_path" ]; then
    ls -lh "$rpm_path" "$rpm_path.sha256"
fi
