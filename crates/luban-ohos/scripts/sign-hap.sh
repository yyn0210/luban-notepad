#!/bin/sh
# Sign a HAP with the OpenHarmony SDK test profile and add ACL permissions.
#
# This path is for emulators and OpenHarmony development images. Retail
# HarmonyOS devices and AppGallery releases need a certificate and profile
# issued by AppGallery Connect; this script cannot mint those.
#
# Required (environment or options):
#   OHOS_SDK_LIB                 SDK toolchains/lib directory
#   OHOS_SIGN_STORE_PASSWORD     keystore password
#
# Optional environment:
#   OHOS_SIGN_KEY_PASSWORD       key password (defaults to the store password)
#   OHOS_HDC                     hdc executable
#   OHOS_SDK_HOME                SDK root, used to find toolchains/lib
#   DEVECO_HOME                  DevEco Studio root, used to find jbr and the SDK
#   JAVA_HOME                    JDK root, used to find java/keytool
#   PYTHON                       python3 or python
#
# Usage:
#   scripts/sign-hap.sh --sdk-lib <dir>
#   scripts/sign-hap.sh --sdk-lib <dir> --install
#
# The default ACL is ohos.permission.READ_PASTEBOARD. Signing a HAP that is
# already installed with a different (or no) signature requires uninstalling
# it first; --install does that and therefore discards application data.
set -eu

crate_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
default_hap="$crate_dir/ohos/entry/build/default/outputs/default/entry-default-unsigned.hap"
default_out="$crate_dir/ohos/entry/build/default/outputs/default/entry-default-signed.hap"

SDK_LIB=${OHOS_SDK_LIB:-}
STORE_PWD=${OHOS_SIGN_STORE_PASSWORD:-}
KEY_PWD=${OHOS_SIGN_KEY_PASSWORD:-}
JAVA_CMD=
KEYTOOL_CMD=
HDC_CMD=${OHOS_HDC:-}
PYTHON_CMD=${PYTHON:-}
HAP=$default_hap
OUT=$default_out
BUNDLE=
ACLS=ohos.permission.READ_PASTEBOARD
VALIDITY_DAYS=3650
COMPAT=
KEYSTORE=
PROFILE_TEMPLATE=
PROFILE_CERT=
PROFILE_KEY_ALIAS="openharmony application profile release"
APP_KEY_ALIAS="openharmony application release"
SIGN_ALG=SHA256withECDSA
DEVICE=
INSTALL=0
KEEP_WORKDIR=0

die() {
    printf '%s\n' "error: $*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Sign a HAP with the OpenHarmony SDK test profile and ACL permissions.

Required:
  --sdk-lib <dir>            SDK toolchains/lib directory
                             (or set OHOS_SDK_LIB / OHOS_SDK_HOME / DEVECO_HOME)
  --store-password <pwd>     keystore password
                             (or set OHOS_SIGN_STORE_PASSWORD)

Options:
  --hap <file>               unsigned HAP (default: entry-default-unsigned.hap)
  --out <file>               signed HAP (default: entry-default-signed.hap)
  --bundle <name>            bundle name (default: read AppScope/app.json5)
  --acls <list>              comma-separated ACL permissions
                             (default: ohos.permission.READ_PASTEBOARD)
  --validity-days <n>        profile validity in days (default: 3650)
  --compatible-version <n>   min API level (default: read from the HAP)
  --key-password <pwd>       key password (default: the store password)
  --keystore <file>          signing keystore (default: SDK OpenHarmony.p12)
  --profile-template <file>  unsigned profile JSON
                             (default: SDK UnsgnedReleasedProfileTemplate.json)
  --profile-cert <file>      profile signing certificate chain
                             (default: SDK OpenHarmonyProfileRelease.pem)
  --profile-key-alias <a>    profile key alias
  --app-key-alias <a>        application key alias
  --java <file>              java executable
  --keytool <file>           keytool executable
  --hdc <file>               hdc executable
  --device <serial>          hdc target serial
  --install                  uninstall the bundle, then install the signed HAP
                             (this discards application data)
  --keep-workdir             keep the temporary profile/certificate directory
  -h, --help                 show this help

Only use this script with the SDK test keystore on emulators or OpenHarmony
development images. Do not commit keystores, passwords, or generated profiles.
EOF
}

need_value() {
    [ "$#" -ge 2 ] || die "option $1 requires a value"
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --sdk-lib) need_value "$@"; SDK_LIB=$2; shift 2 ;;
        --hap) need_value "$@"; HAP=$2; shift 2 ;;
        --out) need_value "$@"; OUT=$2; shift 2 ;;
        --bundle) need_value "$@"; BUNDLE=$2; shift 2 ;;
        --acls) need_value "$@"; ACLS=$2; shift 2 ;;
        --validity-days) need_value "$@"; VALIDITY_DAYS=$2; shift 2 ;;
        --compatible-version) need_value "$@"; COMPAT=$2; shift 2 ;;
        --store-password) need_value "$@"; STORE_PWD=$2; shift 2 ;;
        --key-password) need_value "$@"; KEY_PWD=$2; shift 2 ;;
        --keystore) need_value "$@"; KEYSTORE=$2; shift 2 ;;
        --profile-template) need_value "$@"; PROFILE_TEMPLATE=$2; shift 2 ;;
        --profile-cert) need_value "$@"; PROFILE_CERT=$2; shift 2 ;;
        --profile-key-alias) need_value "$@"; PROFILE_KEY_ALIAS=$2; shift 2 ;;
        --app-key-alias) need_value "$@"; APP_KEY_ALIAS=$2; shift 2 ;;
        --java) need_value "$@"; JAVA_CMD=$2; shift 2 ;;
        --keytool) need_value "$@"; KEYTOOL_CMD=$2; shift 2 ;;
        --hdc) need_value "$@"; HDC_CMD=$2; shift 2 ;;
        --device) need_value "$@"; DEVICE=$2; shift 2 ;;
        --install) INSTALL=1; shift ;;
        --keep-workdir) KEEP_WORKDIR=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

if [ -z "$SDK_LIB" ]; then
    if [ -n "${OHOS_SDK_HOME:-}" ] && [ -d "$OHOS_SDK_HOME/toolchains/lib" ]; then
        SDK_LIB="$OHOS_SDK_HOME/toolchains/lib"
    elif [ -n "${DEVECO_HOME:-}" ] && [ -d "$DEVECO_HOME/sdk/default/openharmony/toolchains/lib" ]; then
        SDK_LIB="$DEVECO_HOME/sdk/default/openharmony/toolchains/lib"
    else
        die "set --sdk-lib, OHOS_SDK_LIB, OHOS_SDK_HOME, or DEVECO_HOME"
    fi
fi
[ -d "$SDK_LIB" ] || die "SDK toolchains/lib not found: $SDK_LIB"

[ -n "$STORE_PWD" ] || die "set OHOS_SIGN_STORE_PASSWORD or --store-password"
[ -n "$KEY_PWD" ] || KEY_PWD=$STORE_PWD
[ -f "$HAP" ] || die "unsigned HAP not found: $HAP (build it with hvigorw assembleHap)"
[ "$HAP" != "$OUT" ] || die "--out must differ from --hap"

if [ -z "$KEYSTORE" ]; then
    KEYSTORE="$SDK_LIB/OpenHarmony.p12"
fi
if [ -z "$PROFILE_TEMPLATE" ]; then
    PROFILE_TEMPLATE="$SDK_LIB/UnsgnedReleasedProfileTemplate.json"
fi
if [ -z "$PROFILE_CERT" ]; then
    PROFILE_CERT="$SDK_LIB/OpenHarmonyProfileRelease.pem"
fi
[ -f "$SDK_LIB/hap-sign-tool.jar" ] || die "hap-sign-tool.jar not found in $SDK_LIB"
[ -f "$KEYSTORE" ] || die "keystore not found: $KEYSTORE"
[ -f "$PROFILE_TEMPLATE" ] || die "profile template not found: $PROFILE_TEMPLATE"
[ -f "$PROFILE_CERT" ] || die "profile certificate not found: $PROFILE_CERT"

if [ -z "$PYTHON_CMD" ]; then
    if command -v python3 >/dev/null 2>&1; then
        PYTHON_CMD=python3
    elif command -v python >/dev/null 2>&1; then
        PYTHON_CMD=python
    else
        die "python3 or python is required"
    fi
fi
"$PYTHON_CMD" -c 'import json, sys, uuid' >/dev/null 2>&1 \
    || die "python is not usable: $PYTHON_CMD"

# Derive the DevEco root from <sdk>/default/openharmony/toolchains/lib when the
# caller did not provide it. This is only a convenience; --java/--keytool win.
DEVECO_DIR=${DEVECO_HOME:-}
if [ -z "$DEVECO_DIR" ]; then
    candidate=$(CDPATH= cd -- "$SDK_LIB/../../../../.." 2>/dev/null && pwd) || candidate=
    if [ -n "$candidate" ] && { [ -x "$candidate/jbr/bin/java" ] || [ -x "$candidate/jbr/bin/java.exe" ]; }; then
        DEVECO_DIR=$candidate
    fi
fi

resolve_cmd() {
    if [ -x "$1" ]; then
        printf '%s\n' "$1"
        return 0
    fi
    command -v "$1" 2>/dev/null
}

if [ -z "$JAVA_CMD" ]; then
    if [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/java" ]; then
        JAVA_CMD="$JAVA_HOME/bin/java"
    elif [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/java.exe" ]; then
        JAVA_CMD="$JAVA_HOME/bin/java.exe"
    elif [ -n "$DEVECO_DIR" ] && [ -x "$DEVECO_DIR/jbr/bin/java" ]; then
        JAVA_CMD="$DEVECO_DIR/jbr/bin/java"
    elif [ -n "$DEVECO_DIR" ] && [ -x "$DEVECO_DIR/jbr/bin/java.exe" ]; then
        JAVA_CMD="$DEVECO_DIR/jbr/bin/java.exe"
    else
        JAVA_CMD=java
    fi
fi
JAVA_CMD=$(resolve_cmd "$JAVA_CMD") || die "java not found; set --java or JAVA_HOME"

if [ -z "$KEYTOOL_CMD" ]; then
    if [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/keytool" ]; then
        KEYTOOL_CMD="$JAVA_HOME/bin/keytool"
    elif [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/keytool.exe" ]; then
        KEYTOOL_CMD="$JAVA_HOME/bin/keytool.exe"
    elif [ -n "$DEVECO_DIR" ] && [ -x "$DEVECO_DIR/jbr/bin/keytool" ]; then
        KEYTOOL_CMD="$DEVECO_DIR/jbr/bin/keytool"
    elif [ -n "$DEVECO_DIR" ] && [ -x "$DEVECO_DIR/jbr/bin/keytool.exe" ]; then
        KEYTOOL_CMD="$DEVECO_DIR/jbr/bin/keytool.exe"
    else
        KEYTOOL_CMD=keytool
    fi
fi
KEYTOOL_CMD=$(resolve_cmd "$KEYTOOL_CMD") || die "keytool not found; set --keytool or JAVA_HOME"

if [ -z "$BUNDLE" ]; then
    app_json="$crate_dir/ohos/AppScope/app.json5"
    [ -f "$app_json" ] || die "bundle name is unset and $app_json does not exist"
    BUNDLE=$("$PYTHON_CMD" - "$app_json" <<'PY'
import re
import sys

text = open(sys.argv[1], encoding="utf-8").read()
match = re.search(r'"bundleName"\s*:\s*"([^"]+)"', text)
if not match:
    raise SystemExit("bundleName not found")
print(match.group(1))
PY
    ) || die "could not read bundleName from $app_json"
fi

if [ -z "$COMPAT" ]; then
    COMPAT=$("$PYTHON_CMD" - "$HAP" <<'PY'
import json
import sys
import zipfile

try:
    with zipfile.ZipFile(sys.argv[1]) as hap:
        pack = json.loads(hap.read("pack.info").decode("utf-8"))
    print(int(pack["summary"]["modules"][0]["apiVersion"]["compatible"]))
except Exception:
    print(23)
PY
    )
fi
case "$COMPAT" in
    ''|*[!0-9]*) die "invalid compatible version: $COMPAT" ;;
esac

mkdir -p "$(dirname "$OUT")"

WORK=$(mktemp -d 2>/dev/null || mktemp -d -t luban-sign) \
    || die "could not create a temporary directory"
if [ "$KEEP_WORKDIR" = 1 ]; then
    printf '%s\n' "work directory: $WORK"
else
    trap 'rm -rf "$WORK"' EXIT
    trap 'rm -rf "$WORK"; exit 1' INT TERM
fi

"$PYTHON_CMD" - "$PROFILE_TEMPLATE" "$WORK/profile.json" "$BUNDLE" "$ACLS" "$VALIDITY_DAYS" <<'PY'
import json
import sys
import time
import uuid

template, out, bundle, acls, days = sys.argv[1:6]
with open(template, encoding="utf-8") as handle:
    profile = json.load(handle)

allowed = [item.strip() for item in acls.split(",") if item.strip()]
if not allowed:
    raise SystemExit("empty ACL list")

profile["uuid"] = str(uuid.uuid4())
now = int(time.time())
profile["validity"] = {
    "not-before": now - 86400,
    "not-after": now + int(days) * 86400,
}
profile.setdefault("bundle-info", {})["bundle-name"] = bundle
profile["acls"] = {"allowed-acls": allowed}
profile.setdefault("permissions", {})["restricted-permissions"] = []

with open(out, "w", encoding="utf-8") as handle:
    json.dump(profile, handle, ensure_ascii=False, indent=2)
PY

"$PYTHON_CMD" - "$WORK/profile.json" "$WORK/app-leaf.pem" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    profile = json.load(handle)
info = profile.get("bundle-info") or {}
certificate = info.get("distribution-certificate") or info.get("development-certificate")
if not certificate:
    raise SystemExit("profile has no application certificate")
with open(sys.argv[2], "w", encoding="ascii") as handle:
    handle.write(certificate)
PY

"$KEYTOOL_CMD" -exportcert -rfc -alias "openharmony application ca" \
    -keystore "$KEYSTORE" -storepass "$STORE_PWD" -file "$WORK/ca.pem" >/dev/null
"$KEYTOOL_CMD" -exportcert -rfc -alias "openharmony application root ca" \
    -keystore "$KEYSTORE" -storepass "$STORE_PWD" -file "$WORK/root.pem" >/dev/null

cat "$WORK/app-leaf.pem" > "$WORK/app-chain.pem"
printf '\n' >> "$WORK/app-chain.pem"
cat "$WORK/ca.pem" >> "$WORK/app-chain.pem"
printf '\n' >> "$WORK/app-chain.pem"
cat "$WORK/root.pem" >> "$WORK/app-chain.pem"

"$JAVA_CMD" -jar "$SDK_LIB/hap-sign-tool.jar" sign-profile \
    -mode localSign \
    -keyAlias "$PROFILE_KEY_ALIAS" \
    -keyPwd "$KEY_PWD" \
    -profileCertFile "$PROFILE_CERT" \
    -inFile "$WORK/profile.json" \
    -signAlg "$SIGN_ALG" \
    -keystoreFile "$KEYSTORE" \
    -keystorePwd "$STORE_PWD" \
    -outFile "$WORK/profile.p7b"

"$JAVA_CMD" -jar "$SDK_LIB/hap-sign-tool.jar" verify-profile \
    -inFile "$WORK/profile.p7b" \
    -outFile "$WORK/verify-profile.json" >/dev/null

"$PYTHON_CMD" - "$WORK/verify-profile.json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    result = json.load(handle)
if result.get("verifiedPassed") is not True:
    raise SystemExit("signed profile verification failed")
PY

"$JAVA_CMD" -jar "$SDK_LIB/hap-sign-tool.jar" sign-app \
    -mode localSign \
    -keyAlias "$APP_KEY_ALIAS" \
    -keyPwd "$KEY_PWD" \
    -appCertFile "$WORK/app-chain.pem" \
    -profileFile "$WORK/profile.p7b" \
    -inFile "$HAP" \
    -signAlg "$SIGN_ALG" \
    -keystoreFile "$KEYSTORE" \
    -keystorePwd "$STORE_PWD" \
    -outFile "$OUT" \
    -compatibleVersion "$COMPAT" \
    -signCode 1

[ -f "$OUT" ] || die "sign-app did not produce $OUT"

printf '%s\n' "signed HAP: $OUT"
printf '%s\n' "bundle: $BUNDLE"
printf '%s\n' "ACLs: $ACLS"

if [ "$INSTALL" = 1 ]; then
    if [ -z "$HDC_CMD" ]; then
        for candidate in "$SDK_LIB/../hdc" "$SDK_LIB/../hdc.exe"; do
            if [ -x "$candidate" ]; then
                HDC_CMD=$candidate
                break
            fi
        done
    fi
    if [ -z "$HDC_CMD" ]; then
        HDC_CMD=$(resolve_cmd hdc) || die "hdc not found; set --hdc or OHOS_HDC"
    fi

    hdc_cmd() {
        if [ -n "$DEVICE" ]; then
            "$HDC_CMD" -t "$DEVICE" "$@"
        else
            "$HDC_CMD" "$@"
        fi
    }

    printf '%s\n' "uninstalling $BUNDLE (application data will be lost)"
    hdc_cmd uninstall "$BUNDLE" || true

    # Windows hdc.exe does not understand MSYS/Git Bash paths.
    install_path=$OUT
    if command -v cygpath >/dev/null 2>&1; then
        case "$(uname -s 2>/dev/null || true)" in
            MINGW*|MSYS*|CYGWIN*) install_path=$(cygpath -w "$OUT") ;;
        esac
    fi
    hdc_cmd install "$install_path"
else
    printf '%s\n' "install with:"
    printf '%s\n' "  hdc uninstall $BUNDLE   # only when replacing an unsigned/other-signature install"
    printf '%s\n' "  hdc install $OUT"
fi
