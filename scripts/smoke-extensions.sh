#!/usr/bin/env bash
# End-to-end smoke coverage for the 1.0 extension surface.
#
# Requires a built `target/debug/luban-notepad` (or LUBAN_BIN) and a display.
# CI runs it on Xvfb; locally it can use an existing DISPLAY.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${LUBAN_BIN:-$ROOT/target/debug/luban-notepad}"
if [ ! -x "$BIN" ]; then
  (cd "$ROOT" && cargo build -p luban-app)
fi

if command -v xvfb-run >/dev/null 2>&1; then
  RUNNER=(xvfb-run -a -s "-screen 0 1440x900x24")
elif [ -n "${DISPLAY:-}" ]; then
  RUNNER=()
else
  echo "smoke-extensions: no display and no xvfb-run; skipping GUI smoke" >&2
  exit 0
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-ext-smoke.XXXXXX")"
PRIMARY=""
cleanup() {
  if [ -n "$PRIMARY" ] && kill -0 "$PRIMARY" 2>/dev/null; then
    kill "$PRIMARY" 2>/dev/null || true
    sleep 0.2
    kill -9 "$PRIMARY" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

export LUBAN_NOTEPAD_CONFIG_DIR="$TMP"
export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"

mkdir -p "$TMP/plugins/com.example.smoke" "$TMP/packages/syntaxes" \
  "$TMP/packages/themes" "$TMP/packages/snippets"
cat > "$TMP/plugins/com.example.smoke/plugin.json" <<'JSON'
{
  "id": "com.example.smoke",
  "name": "Smoke",
  "version": "1.0.0",
  "api_version": 1,
  "main": "main.lua",
  "activate": "activate",
  "contributes": {
    "commands": [
      { "id": "smoke.run", "label": "Smoke: Run", "entry": "run", "when": "edit_text" },
      { "id": "smoke.fail", "label": "Smoke: Fail", "entry": "fail", "when": "edit_text" }
    ],
    "status_bar": [
      { "id": "smoke.status", "text": "smoke", "alignment": "left" }
    ],
    "settings": [
      { "key": "text", "type": "string", "label": "Text", "default": "SMOKE_TEXT" }
    ],
    "panels": [
      { "id": "smoke.panel", "title": "Smoke panel", "entry": "panel", "placement": "right" }
    ]
  }
}
JSON
cat > "$TMP/plugins/com.example.smoke/main.lua" <<'LUA'
return {
  activate = function(ctx) luban.log("info", "SMOKE_PLUGIN_ACTIVATED") end,
  run = function(ctx)
    local document = ctx.document
    luban.log("info", "SMOKE_PLUGIN_RAN")
    luban.log("info", "SMOKE_DOC " .. tostring(document and document.line) .. ":" .. tostring(document and document.column))
    luban.notify("SMOKE_NOTIFY")
    luban.insert_text(luban.settings.get("text"))
    luban.status.set("smoke.status", "smoke-ran")
  end,
  fail = function(ctx) error("smoke failure") end,
  panel = function(ctx)
    return {
      title = "Smoke panel",
      lines = {
        { text = "Run", command = "smoke.run" },
        "Read-only line"
      }
    }
  end,
}
LUA
EMPTY_HASH="$(printf '' | sha256sum | awk '{print $1}')"
cat > "$TMP/plugins/.installed.json" <<JSON
{
  "schema": 1,
  "plugins": {
    "com.example.smoke": {
      "id": "com.example.smoke",
      "version": "1.0.0",
      "source": { "type": "local", "path": "$TMP/plugins/com.example.smoke" },
      "installed_at": 0,
      "approved_permissions_hash": "$EMPTY_HASH"
    }
  }
}
JSON
cat > "$TMP/packages/syntaxes/smoke.sublime-syntax" <<'YAML'
%YAML 1.2
---
name: SmokeLang
file_extensions: [smk]
scope: source.smk
first_line_match: '^#!/usr/bin/env smoke'
contexts:
  main:
    - match: '\b(if|else)\b'
      scope: keyword.control.smk
YAML
cat > "$TMP/packages/snippets/demo.sublime-snippet" <<'XML'
<snippet>
  <content><![CDATA[println!("${1:value}");]]></content>
  <tabTrigger>pl</tabTrigger>
  <scope>source.rust</scope>
  <description>Print line</description>
</snippet>
XML
cat > "$TMP/packages/themes/smoke.tmTheme" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>name</key><string>Smoke Theme</string>
  <key>settings</key>
  <array>
    <dict><key>settings</key><dict>
      <key>background</key><string>#ffffff</string>
      <key>foreground</key><string>#000000</string>
    </dict></dict>
  </array>
</dict>
</plist>
XML
printf 'fn main() {}\n' > "$TMP/sample.rs"
printf 'a\nb\nc\n' > "$TMP/left.txt"
printf 'a\nB\nc\n' > "$TMP/right.txt"

run_smoke() {
  local name="$1"
  shift
  if ! "${RUNNER[@]}" timeout 30 "$BIN" --smoke-test "$@" >/dev/null 2>&1; then
    echo "smoke-extensions: $name failed" >&2
    sed -n '1,160p' "$TMP/logs/luban.log" >&2 || true
    exit 1
  fi
}

echo "smoke-extensions: plugin command"
run_smoke "plugin command" --goto "$TMP/sample.rs:2:1" --command smoke.run
grep -q "SMOKE_PLUGIN_RAN" "$TMP/logs/luban.log"
grep -q "SMOKE_DOC 2:1" "$TMP/logs/luban.log"

echo "smoke-extensions: manager, panel, snippet picker, diff"
run_smoke "plugin manager" --command plugins.manage
run_smoke "plugin panel" --command plugins.open-panel
run_smoke "snippet picker" "$TMP/sample.rs" --command edit.insert-snippet
run_smoke "diff" --diff "$TMP/left.txt" "$TMP/right.txt"
rm -f "$TMP/logs/luban.log"
run_smoke "goto clamp" --goto "$TMP/sample.rs:999:999"
grep -q "was clamped" "$TMP/logs/luban.log"

echo "smoke-extensions: safe mode disables plugins"
rm -f "$TMP/logs/luban.log"
set +e
"${RUNNER[@]}" timeout 30 "$BIN" --smoke-test --safe-mode --command smoke.run >/dev/null 2>&1
SAFE=$?
set -e
if [ "$SAFE" -ne 3 ]; then
  echo "smoke-extensions: safe mode expected exit 3, got $SAFE" >&2
  exit 1
fi
if grep -q "SMOKE_PLUGIN_ACTIVATED" "$TMP/logs/luban.log"; then
  echo "smoke-extensions: safe mode loaded the plugin" >&2
  exit 1
fi

echo "smoke-extensions: single instance and wait"
"${RUNNER[@]}" "$BIN" --new >"$TMP/primary.out" 2>&1 &
PRIMARY=$!
for _ in $(seq 1 100); do
  [ -f "$TMP/instance.lock" ] && break
  sleep 0.1
done
if [ ! -f "$TMP/instance.lock" ]; then
  echo "smoke-extensions: primary did not start" >&2
  cat "$TMP/primary.out" >&2 || true
  exit 1
fi

timeout 10 "$BIN" --goto "$TMP/sample.rs:2" || exit 1
( timeout 15 "$BIN" --wait --goto "$TMP/sample.rs:2" >/dev/null 2>&1; echo $? > "$TMP/wait.exit" ) &
WAIT_CLIENT=$!
sleep 1
timeout 10 "$BIN" --command file.close || exit 1
wait "$WAIT_CLIENT" 2>/dev/null || true
[ "$(cat "$TMP/wait.exit" 2>/dev/null)" = "0" ] || {
  echo "smoke-extensions: forwarded --wait did not return 0" >&2
  exit 1
}
( timeout 15 "$BIN" --wait --goto "$TMP/sample.rs:2" >/dev/null 2>&1; echo $? > "$TMP/wait-all.exit" ) &
WAIT_ALL=$!
sleep 1
timeout 10 "$BIN" --command file.close-all || exit 1
wait "$WAIT_ALL" 2>/dev/null || true
[ "$(cat "$TMP/wait-all.exit" 2>/dev/null)" = "0" ] || {
  echo "smoke-extensions: --wait did not resolve on close-all" >&2
  exit 1
}
printf 'stdin data\n' | timeout 10 "$BIN" --stdin --command file.close || exit 1
timeout 10 "$BIN" --diff "$TMP/left.txt" "$TMP/right.txt" --command file.close || exit 1
timeout 10 "$BIN" --new-window "$TMP/sample.rs" || exit 1
# Close the window that owns the current focus; the remaining window must keep
# serving IPC requests.
timeout 10 "$BIN" --command file.exit || exit 1
sleep 1
timeout 10 "$BIN" --goto "$TMP/right.txt:1" || exit 1
if ! kill -0 "$PRIMARY" 2>/dev/null; then
  echo "smoke-extensions: primary died after closing one of two windows" >&2
  exit 1
fi
set +e
timeout 10 "$BIN" --wait "$TMP/missing.txt" >/dev/null 2>&1
MISSING_WAIT=$?
set -e
if [ "$MISSING_WAIT" -ne 3 ]; then
  echo "smoke-extensions: --wait missing file expected 3, got $MISSING_WAIT" >&2
  exit 1
fi

set +e
timeout 10 "$BIN" --new-window --command no.such.command "$TMP/sample.rs" >/dev/null 2>&1
BAD=$?
set -e
if [ "$BAD" -ne 3 ]; then
  echo "smoke-extensions: bad --new-window command expected 3, got $BAD" >&2
  exit 1
fi
if ! kill -0 "$PRIMARY" 2>/dev/null; then
  echo "smoke-extensions: primary died after a failed forwarded command" >&2
  exit 1
fi

set +e
timeout 10 "$BIN" --new-window "$TMP/missing.txt" >/dev/null 2>&1
MISSING=$?
timeout 10 "$BIN" --new-window --diff "$TMP/missing.txt" "$TMP/right.txt" >/dev/null 2>&1
MISSING_DIFF=$?
set -e
if [ "$MISSING" -ne 3 ] || [ "$MISSING_DIFF" -ne 3 ]; then
  echo "smoke-extensions: missing new-window targets expected 3, got $MISSING/$MISSING_DIFF" >&2
  exit 1
fi
if ! kill -0 "$PRIMARY" 2>/dev/null; then
  echo "smoke-extensions: primary died after missing new-window targets" >&2
  exit 1
fi

set +e
timeout 10 "$BIN" --new-window --command smoke.fail "$TMP/sample.rs" >/dev/null 2>&1
FAIL_CMD=$?
printf 'secret\n' > "$TMP/unreadable.txt"
chmod 000 "$TMP/unreadable.txt"
timeout 10 "$BIN" --new-window --diff "$TMP/unreadable.txt" "$TMP/right.txt" >/dev/null 2>&1
UNREADABLE=$?
set -e
chmod 644 "$TMP/unreadable.txt"
if [ "$FAIL_CMD" -ne 3 ] || [ "$UNREADABLE" -ne 3 ]; then
  echo "smoke-extensions: new-window failures expected 3, got $FAIL_CMD/$UNREADABLE" >&2
  exit 1
fi
if ! kill -0 "$PRIMARY" 2>/dev/null; then
  echo "smoke-extensions: primary died after new-window failures" >&2
  exit 1
fi

echo "smoke-extensions: plugin settings survive reload while disabled"
A8_TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-ext-a8.XXXXXX")"
cp -r "$TMP/plugins" "$A8_TMP/plugins"
cat > "$A8_TMP/settings.json" <<'JSON'
{
  "disabled_plugins": ["com.example.smoke"],
  "plugin_settings": { "com.example.smoke": { "text": "CUSTOM" } },
  "restore_session": false
}
JSON
LUBAN_NOTEPAD_CONFIG_DIR="$A8_TMP" "${RUNNER[@]}" "$BIN" --new >/dev/null 2>&1 &
A8_PRIMARY=$!
for _ in $(seq 1 100); do
  [ -f "$A8_TMP/instance.lock" ] && break
  sleep 0.1
done
LUBAN_NOTEPAD_CONFIG_DIR="$A8_TMP" timeout 10 "$BIN" --command plugins.reload || exit 1
LUBAN_NOTEPAD_CONFIG_DIR="$A8_TMP" timeout 10 "$BIN" --command file.exit || exit 1
for _ in $(seq 1 50); do
  kill -0 "$A8_PRIMARY" 2>/dev/null || break
  sleep 0.1
done
kill "$A8_PRIMARY" 2>/dev/null || true
if ! grep -q 'CUSTOM' "$A8_TMP/settings.json"; then
  echo "smoke-extensions: disabled plugin settings were cleared on reload" >&2
  cat "$A8_TMP/settings.json" >&2
  rm -rf "$A8_TMP"
  exit 1
fi
rm -rf "$A8_TMP"

echo "smoke-extensions: independent --wait process"
WAIT_TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-ext-wait.XXXXXX")"
LUBAN_NOTEPAD_CONFIG_DIR="$WAIT_TMP" "${RUNNER[@]}" "$BIN" --wait --goto "$TMP/sample.rs:1" >/dev/null 2>&1 &
WAIT_PRIMARY=$!
for _ in $(seq 1 100); do
  [ -f "$WAIT_TMP/instance.lock" ] && break
  sleep 0.1
done
LUBAN_NOTEPAD_CONFIG_DIR="$WAIT_TMP" timeout 10 "$BIN" --command file.close || exit 1
for _ in $(seq 1 50); do
  kill -0 "$WAIT_PRIMARY" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$WAIT_PRIMARY" 2>/dev/null; then
  echo "smoke-extensions: independent --wait process did not exit" >&2
  kill "$WAIT_PRIMARY" 2>/dev/null || true
  rm -rf "$WAIT_TMP"
  exit 1
fi
wait "$WAIT_PRIMARY" 2>/dev/null || true
if [ -e "$WAIT_TMP/instance.lock" ] || [ -e "$WAIT_TMP/instance.sock" ]; then
  echo "smoke-extensions: --wait exit left IPC files behind" >&2
  rm -rf "$WAIT_TMP"
  exit 1
fi
rm -rf "$WAIT_TMP"

echo "smoke-extensions: --wait resolves when its window exits"
CLOSE_TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-ext-close.XXXXXX")"
LUBAN_NOTEPAD_CONFIG_DIR="$CLOSE_TMP" "${RUNNER[@]}" "$BIN" --new >/dev/null 2>&1 &
CLOSE_PRIMARY=$!
for _ in $(seq 1 100); do
  [ -f "$CLOSE_TMP/instance.lock" ] && break
  sleep 0.1
done
LUBAN_NOTEPAD_CONFIG_DIR="$CLOSE_TMP" timeout 10 "$BIN" --goto "$TMP/sample.rs:1" || exit 1
( LUBAN_NOTEPAD_CONFIG_DIR="$CLOSE_TMP" timeout 15 "$BIN" --wait --goto "$TMP/sample.rs:2" >/dev/null 2>&1; echo $? > "$CLOSE_TMP/wait.exit" ) &
CLOSE_CLIENT=$!
sleep 1
LUBAN_NOTEPAD_CONFIG_DIR="$CLOSE_TMP" timeout 10 "$BIN" --command file.exit || exit 1
wait "$CLOSE_CLIENT" 2>/dev/null || true
if [ "$(cat "$CLOSE_TMP/wait.exit" 2>/dev/null)" != "0" ]; then
  echo "smoke-extensions: --wait did not succeed when its window exited" >&2
  kill "$CLOSE_PRIMARY" 2>/dev/null || true
  rm -rf "$CLOSE_TMP"
  exit 1
fi
for _ in $(seq 1 50); do
  kill -0 "$CLOSE_PRIMARY" 2>/dev/null || break
  sleep 0.1
done
if kill -0 "$CLOSE_PRIMARY" 2>/dev/null; then
  echo "smoke-extensions: window close left the process running" >&2
  kill "$CLOSE_PRIMARY" 2>/dev/null || true
  rm -rf "$CLOSE_TMP"
  exit 1
fi
wait "$CLOSE_PRIMARY" 2>/dev/null || true
rm -rf "$CLOSE_TMP"

echo "smoke-extensions: ok"
