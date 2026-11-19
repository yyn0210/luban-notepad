#!/bin/sh
# Build the KaihongOS (深开鸿) market HAP and optionally sign it.
#
# KaihongOS is an OpenHarmony distribution, so its market HAP is built with
# ohos/build-profile.kaihongos.json5 (runtimeOS OpenHarmony) instead of the
# HarmonyOS profile used for AppGallery. The build itself is unsigned;
# signing goes through scripts/sign-hap.sh:
#   - default: SDK test material, for KaihongOS VM validation
#   - --sign -- <release material flags>: market release material, identical
#     to the AGC mode (--profile <p7b> --app-cert <cer> --keystore <p12>
#     --app-key-alias <alias>)
#
# The local ohos/build-profile.json5 usually holds the AppGallery signing
# configuration (gitignored, hand-maintained in DevEco). This script
# snapshots it and restores the exact bytes when the build ends or fails,
# then checks the restored hash, so the AppGallery flow is never disturbed.
#
# Required environment:
#   OHOS_NDK_HOME          NDK root for ohrs
#   OHOS_BASE_SDK_HOME     OpenHarmony SDK root for hvigor in the public-SDK
#                          layout: <root>/<compileSdkVersion>/{ets,js,native,
#                          previewer,toolchains}. Do NOT point it at the
#                          DevEco-managed sdk dir — hvigor 6 rejects that
#                          layout for OpenHarmony products (00308018). A
#                          junction tree over the DevEco components works.
#   HVIGORW                path to hvigorw.js; or set DEVECO_HOME and
#                          tools/hvigor/bin/hvigorw.js is used
#
# Optional environment:
#   DEVECO_SDK_HOME        SDK root; defaults to OHOS_BASE_SDK_HOME (CI sets
#                          both to the same root)
#   OHPM                   ohpm executable (default: ohpm on PATH; skipped
#                          when absent)
#   PYTHON                 python3 or python
#   OHOS_SIGN_*            only needed for --sign (see scripts/sign-hap.sh)
#
# Usage:
#   scripts/build-kaihongos-hap.sh [--app]
#   scripts/build-kaihongos-hap.sh --sign [--app]   # SDK test material
#   scripts/build-kaihongos-hap.sh --sign -- \
#     --profile <kaihong.p7b> --app-cert <kaihong.cer> \
#     --keystore <release.p12> --app-key-alias <alias>
#
# --app builds the project-scope APP container (hvigor assembleApp) instead
# of the module HAP. Markets that upload zip packages want this container:
# it is itself a zip whose root carries pack.info plus the inner HAPs.
#
# Artifacts land in dist-release/kaihongos/ (both arm64-v8a and x86_64), a
# directory separate from the AppGallery outputs.
set -eu

crate_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$crate_dir"

die() {
    printf '%s\n' "error: $*" >&2
    exit 1
}

usage() {
    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'
}

SIGN=0
APP_MODE=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        --sign) SIGN=1; shift ;;
        --app) APP_MODE=1; shift ;;
        --) shift; break ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done
# Anything left in "$@" is forwarded to sign-hap.sh verbatim (release material).

if [ -z "${OHOS_NDK_HOME:-}" ]; then
    die "set OHOS_NDK_HOME (NDK root for ohrs)"
fi
if [ -z "${OHOS_BASE_SDK_HOME:-}" ]; then
    die "set OHOS_BASE_SDK_HOME (OpenHarmony SDK root with a <compileSdkVersion>/ component dir)"
fi
DEVECO_SDK_HOME=${DEVECO_SDK_HOME:-$OHOS_BASE_SDK_HOME}
export DEVECO_SDK_HOME
HVIGORW_PATH=${HVIGORW:-}
if [ -z "$HVIGORW_PATH" ] && [ -n "${DEVECO_HOME:-}" ]; then
    HVIGORW_PATH="$DEVECO_HOME/tools/hvigor/bin/hvigorw.js"
fi
[ -n "$HVIGORW_PATH" ] || die "set HVIGORW to hvigorw.js (or DEVECO_HOME to the DevEco Studio root)"
[ -f "$HVIGORW_PATH" ] || die "hvigorw.js not found: $HVIGORW_PATH"
NODE_CMD=$(command -v node 2>/dev/null) || die "node is required to run hvigorw.js"

if [ -z "${PYTHON_CMD:-}" ]; then
    PYTHON_CMD=${PYTHON:-}
    if [ -z "$PYTHON_CMD" ]; then
        if command -v python3 >/dev/null 2>&1; then
            PYTHON_CMD=python3
        elif command -v python >/dev/null 2>&1; then
            PYTHON_CMD=python
        else
            die "python3 or python is required"
        fi
    fi
fi
"$PYTHON_CMD" -c 'import json, sys, zipfile' >/dev/null 2>&1 \
    || die "python is not usable: $PYTHON_CMD"

WORK=$(mktemp -d 2>/dev/null || mktemp -d -t luban-kaihongos) \
    || die "could not create a temporary directory"
HAD_PROFILE=0
if [ -f ohos/build-profile.json5 ]; then
    HAD_PROFILE=1
    cp ohos/build-profile.json5 "$WORK/profile-before.json5"
    before_hash=$(sha256sum ohos/build-profile.json5 | cut -d' ' -f1)
fi

restore_profile() {
    if [ "$HAD_PROFILE" = 1 ]; then
        cp "$WORK/profile-before.json5" ohos/build-profile.json5
    else
        rm -f ohos/build-profile.json5
    fi
}
trap 'restore_profile; rm -rf "$WORK"' EXIT
trap 'exit 1' INT TERM

# 1. Native libraries (both ABIs) + the KaihongOS profile. build-hap.sh also
#    leaves a .bak of the previous profile; the snapshot above is authoritative.
printf '%s\n' "==> building native libraries (arm64 + x86_64) and staging the KaihongOS profile"
sh "$crate_dir/scripts/build-hap.sh" --profile "$crate_dir/ohos/build-profile.kaihongos.json5" --abi both --release

# 2. Dependencies (best effort; a fresh checkout needs ohpm, an existing tree
#    usually has oh_modules already).
if command -v "${OHPM:-ohpm}" >/dev/null 2>&1; then
    printf '%s\n' "==> ohpm install --all"
    (cd ohos && "${OHPM:-ohpm}" install --all)
else
    printf '%s\n' "==> ohpm not found; using the existing oh_modules"
fi

# 3. Unsigned release artifact (module HAP or project APP container).
if [ "$APP_MODE" = 1 ]; then
    printf '%s\n' "==> hvigor assembleApp (release, project scope)"
    (cd ohos && "$NODE_CMD" "$HVIGORW_PATH" \
        -p product=default -p buildMode=release assembleApp --no-daemon)
    HAP_UNSIGNED="ohos/build/outputs/default/ohos-default-unsigned.app"
    if [ ! -f "$HAP_UNSIGNED" ]; then
        HAP_UNSIGNED=$(ls -t ohos/build/outputs/default/*.app 2>/dev/null | head -1 || true)
    fi
    [ -n "${HAP_UNSIGNED:-}" ] && [ -f "$HAP_UNSIGNED" ] \
        || die "unsigned APP container not found under ohos/build/outputs/default/"
else
    printf '%s\n' "==> hvigor assembleHap (release)"
    (cd ohos && "$NODE_CMD" "$HVIGORW_PATH" --mode module \
        -p product=default -p buildMode=release assembleHap --no-daemon)
    HAP_UNSIGNED="ohos/entry/build/default/outputs/default/entry-default-unsigned.hap"
    [ -f "$HAP_UNSIGNED" ] || die "unsigned HAP not found: $HAP_UNSIGNED"
fi

# 4. Restore the AppGallery profile before anything else, and prove it.
restore_profile
if [ "$HAD_PROFILE" = 1 ]; then
    after_hash=$(sha256sum ohos/build-profile.json5 | cut -d' ' -f1)
    [ "$after_hash" = "$before_hash" ] \
        || die "ohos/build-profile.json5 was not restored byte-identical; recover it from ohos/build-profile.json5.bak"
    printf '%s\n' "==> ohos/build-profile.json5 restored byte-identical (AppGallery signing config intact)"
else
    printf '%s\n' "==> ohos/build-profile.json5 removed again (it did not exist before)"
fi

# 5. Package sanity: Release stage, compatible 15, target 26, both ABIs.
if [ "$APP_MODE" = 1 ]; then
    mode=app
else
    mode=hap
fi
printf '%s\n' "==> verifying pack.info and contents ($mode)"
"$PYTHON_CMD" - "$mode" "$HAP_UNSIGNED" <<'PY'
import json
import sys
import zipfile

mode = sys.argv[1]
problems = []
with zipfile.ZipFile(sys.argv[2]) as pkg:
    names = set(pkg.namelist())
    pack = json.loads(pkg.read("pack.info").decode("utf-8"))
    api = pack["summary"]["modules"][0]["apiVersion"]
    release = api.get("releaseType")
    compatible = int(str(api.get("compatible")).split(".")[0])
    target = int(str(api.get("target")).split(".")[0])

    if release != "Release":
        problems.append(f"releaseType={release!r} (expect Release; Beta packages get rejected)")
    if compatible != 15:
        problems.append(f"compatible={compatible} (expect 15)")
    if target != 26:
        problems.append(f"target={target} (expect 26)")
    if mode == "app":
        haps = [n for n in names if n.endswith(".hap")]
        if not haps:
            problems.append("no HAP found inside the container")
        for hap_name in haps:
            with pkg.open(hap_name) as inner:
                with zipfile.ZipFile(inner) as hap:
                    hap_names = set(hap.namelist())
            for abi in ("arm64-v8a", "x86_64"):
                for lib in (f"libs/{abi}/libluban_ohos.so", f"libs/{abi}/libc++_shared.so"):
                    if lib not in hap_names:
                        problems.append(f"{hap_name}: missing {lib}")
    else:
        for abi in ("arm64-v8a", "x86_64"):
            for lib in (f"libs/{abi}/libluban_ohos.so", f"libs/{abi}/libc++_shared.so"):
                if lib not in names:
                    problems.append(f"missing {lib}")
if problems:
    sys.stderr.write("\n".join(problems) + "\n")
    raise SystemExit(1)
if mode == "app":
    print("pack.info ok: releaseType=Release, compatible=15, target=26; container HAP carries both ABIs")
else:
    print("pack.info ok: releaseType=Release, compatible=15, target=26; arm64-v8a + x86_64 present")
PY

# 6. Stage the artifact under dist-release/kaihongos/.
version=$("$PYTHON_CMD" - ohos/AppScope/app.json5 <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'"versionName"\s*:\s*"([^"]+)"', text)
if not match:
    raise SystemExit("versionName not found")
print(match.group(1))
PY
)
out_dir="$crate_dir/dist-release/kaihongos"
mkdir -p "$out_dir"
if [ "$APP_MODE" = 1 ]; then
    ext=app
else
    ext=hap
fi
staged_unsigned="$out_dir/luban-notepad-$version-kaihongos-unsigned.$ext"
cp "$HAP_UNSIGNED" "$staged_unsigned"
printf '%s\n' "==> staged: $staged_unsigned"

# 7. Optional signing.
if [ "$SIGN" = 1 ]; then
    staged_signed="$out_dir/luban-notepad-$version-kaihongos-signed.$ext"
    printf '%s\n' "==> signing via scripts/sign-hap.sh"
    sh "$crate_dir/scripts/sign-hap.sh" --hap "$staged_unsigned" --out "$staged_signed" "$@"
    printf '%s\n' "==> staged: $staged_signed"
fi

if [ "$APP_MODE" = 1 ]; then
    printf '%s\n' "done. the signed .app is itself a zip (pack.info + HAPs at its root):"
    printf '%s\n' "  markets that upload zip packages take it directly (rename to .zip if needed)."
else
    printf '%s\n' "done. install on a KaihongOS device/VM with:"
    printf '%s\n' "  hdc install <signed.hap>   # uninstall first when replacing a differently signed package"
fi
