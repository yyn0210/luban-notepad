#!/usr/bin/env bash
# Keyboard-and-mouse integration tests for the main editor window.
#
# The script drives a real app window with xdotool on Xvfb and asserts on
# observable results: saved file contents, window titles and process state. It
# does not compare screenshots. On failure it prints the failing step, the tail
# of the application log and (when available) a screenshot path.
#
# Usage:
#   scripts/gui-integration.sh [--require-display]
#
# `--require-display` makes a missing display a failure instead of a skip; CI
# uses it so "skipped" can never be mistaken for "passed".
#
# Environment:
#   LUBAN_BIN            app binary (default: $CARGO_TARGET_DIR/debug/luban-notepad)
#   LUBAN_GUI_ARTIFACTS  directory for failure screenshots and application logs
#   LUBAN_GUI_DISPLAY    reuse this display instead of starting a private Xvfb
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${LUBAN_BIN:-$TARGET_DIR/debug/luban-notepad}"
ARTIFACTS="${LUBAN_GUI_ARTIFACTS:-}"
REQUIRE_DISPLAY=0
for arg in "$@"; do
  case "$arg" in
    --require-display) REQUIRE_DISPLAY=1 ;;
    -h|--help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "gui-integration: unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

if ! command -v xdotool >/dev/null 2>&1; then
  if [ "$REQUIRE_DISPLAY" = "1" ]; then
    echo "gui-integration: xdotool is required but was not found" >&2
    exit 1
  fi
  echo "gui-integration: SKIP (xdotool not installed)" >&2
  exit 0
fi

# GUI tests always run on a private Xvfb server so they never move the pointer
# or open windows on the user's desktop. Set LUBAN_GUI_DISPLAY to reuse an
# explicitly chosen display instead.
if [ -n "${LUBAN_GUI_DISPLAY:-}" ]; then
  export DISPLAY="$LUBAN_GUI_DISPLAY"
elif [ "${LUBAN_GUI_UNDER_XVFB:-0}" = "1" ] && xdotool getdisplaygeometry >/dev/null 2>&1; then
  : # xvfb-run already provided the private display
elif command -v xvfb-run >/dev/null 2>&1; then
  export LUBAN_GUI_UNDER_XVFB=1
  exec xvfb-run -a -s "-screen 0 1440x900x24" "$0" "$@"
else
  XVFB_BIN="$(command -v Xvfb || true)"
  if [ -z "$XVFB_BIN" ] && [ -x "$HOME/.local/xvfb-root/usr/bin/Xvfb" ]; then
    XVFB_BIN="$HOME/.local/xvfb-root/usr/bin/Xvfb"
    if [ -d "$HOME/.local/xvfb-root/usr/share/X11/xkb" ]; then
      export XKB_CONFIG_ROOT="${XKB_CONFIG_ROOT:-$HOME/.local/xvfb-root/usr/share/X11/xkb}"
    fi
  fi
  if [ -n "$XVFB_BIN" ]; then
    if [ -n "${LUBAN_GUI_XVFB_PID:-}" ]; then
      echo "gui-integration: Xvfb did not provide a usable display" >&2
      exit 1
    fi
    "$XVFB_BIN" :97 -screen 0 1440x900x24 -nolisten tcp >/dev/null 2>&1 &
    export LUBAN_GUI_XVFB_PID=$!
    export DISPLAY=:97
    for _ in $(seq 1 50); do
      xdotool getdisplaygeometry >/dev/null 2>&1 && break
      sleep 0.1
    done
  fi
fi

if [ -z "${DISPLAY:-}" ] || ! xdotool getdisplaygeometry >/dev/null 2>&1; then
  if [ "$REQUIRE_DISPLAY" = "1" ]; then
    echo "gui-integration: no usable display and xvfb-run/Xvfb unavailable" >&2
    exit 1
  fi
  echo "gui-integration: SKIP (no display)" >&2
  exit 0
fi

if [ ! -x "$BIN" ]; then
  echo "gui-integration: building $BIN"
  (cd "$ROOT" && cargo build -p luban-app)
fi
if [ ! -x "$BIN" ]; then
  echo "gui-integration: missing app binary at $BIN" >&2
  exit 1
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-gui.XXXXXX")"
STEP="startup"
APP_PID=""

cleanup() {
  if [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
    sleep 0.3
    kill -9 "$APP_PID" 2>/dev/null || true
  fi
  if [ -n "${LUBAN_GUI_XVFB_PID:-}" ] && kill -0 "$LUBAN_GUI_XVFB_PID" 2>/dev/null; then
    kill "$LUBAN_GUI_XVFB_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"
export XDG_RUNTIME_DIR="$TMP/xdg"
export LUBAN_NOTEPAD_CONFIG_DIR="$TMP/config"
mkdir -p "$XDG_RUNTIME_DIR" "$LUBAN_NOTEPAD_CONFIG_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

LOG="$LUBAN_NOTEPAD_CONFIG_DIR/logs/luban.log"
WID=""

screenshot() {
  local name="$1"
  command -v ffmpeg >/dev/null 2>&1 || return 0
  local dir="${ARTIFACTS:-$TMP}"
  mkdir -p "$dir"
  local out="$dir/$name.png"
  ffmpeg -loglevel error -f x11grab -video_size 1440x900 -i "${DISPLAY}.0" \
    -frames:v 1 -y "$out" >/dev/null 2>&1 || return 0
  echo "$out"
}

fail() {
  local reason="$1"
  echo "gui-integration: FAILED at step '$STEP': $reason" >&2
  local shot
  shot="$(screenshot "failure-$(echo "$STEP" | tr ' /' '--')")"
  [ -n "$shot" ] && echo "gui-integration: screenshot: $shot" >&2
  if [ -f "$LOG" ]; then
    echo "gui-integration: application log: $LOG" >&2
    echo "gui-integration: application log (last 40 lines):" >&2
    tail -40 "$LOG" >&2
    if [ -n "$ARTIFACTS" ]; then
      mkdir -p "$ARTIFACTS"
      cp "$LOG" "$ARTIFACTS/application.log" 2>/dev/null || true
    fi
  fi
  exit 1
}

step() {
  STEP="$1"
  echo "gui-integration: $STEP"
}

window_title() {
  xdotool getwindowname "$WID" 2>/dev/null || true
}

key() {
  xdotool key --window "$WID" --clearmodifiers "$1"
}

type_text() {
  xdotool type --window "$WID" --delay 25 -- "$1"
}

wait_for_title() {
  local expected="$1" limit="${2:-10}" i
  for i in $(seq 1 $((limit * 10))); do
    [ "$(window_title)" = "$expected" ] && return 0
    sleep 0.1
  done
  return 1
}

wait_for_title_contains() {
  local needle="$1" limit="${2:-10}" i
  for i in $(seq 1 $((limit * 10))); do
    case "$(window_title)" in
      *"$needle"*) return 0 ;;
    esac
    sleep 0.1
  done
  return 1
}

wait_for_file_contains() {
  local file="$1" needle="$2" limit="${3:-10}" i
  for i in $(seq 1 $((limit * 10))); do
    if grep -qF -- "$needle" "$file" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

editor_click() {
  # Click well inside the text area, below the menu, toolbar, breadcrumbs and
  # tab strip. The window is placed at the top-left of the Xvfb screen.
  xdotool mousemove --window "$WID" 300 500 click 1
  sleep 0.2
}

SAMPLE="$TMP/sample.txt"
SECOND="$TMP/second.txt"
python3 - "$SAMPLE" <<'PY'
import sys
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    for index in range(1, 201):
        handle.write(f"line {index:03d}\n")
PY
printf 'second file\n' > "$SECOND"

step "launch"
"$BIN" "$SAMPLE" >"$TMP/app.out" 2>&1 &
APP_PID=$!
for _ in $(seq 1 150); do
  WID="$(xdotool search --onlyvisible --name "Luban Notepad" 2>/dev/null | head -1 || true)"
  [ -n "$WID" ] && break
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    fail "the application exited during startup"
  fi
  sleep 0.1
done
[ -n "$WID" ] || fail "no editor window appeared"
xdotool windowfocus "$WID" 2>/dev/null || true
wait_for_title "sample.txt - Luban Notepad" 20 || fail "unexpected start title: $(window_title)"

step "mouse click positions the caret and Ctrl+S saves"
editor_click
type_text "MARKER"
sleep 0.3
key ctrl+s
wait_for_file_contains "$SAMPLE" "MARKER" 10 || fail "Ctrl+S did not write MARKER"
MARKER_LINE="$(grep -nF "MARKER" "$SAMPLE" | head -1 | cut -d: -f1)"
[ -n "$MARKER_LINE" ] || fail "MARKER is missing from the saved file"
[ "$MARKER_LINE" -ne 1 ] || fail "the mouse click did not move the caret away from line 1"

step "Ctrl+F selects a match and typing replaces it"
key ctrl+f
sleep 0.6
type_text "line 042"
sleep 0.3
key Return
sleep 0.4
key Escape
sleep 0.3
type_text "REPLACED"
sleep 0.3
key ctrl+s
wait_for_file_contains "$SAMPLE" "REPLACED" 10 || fail "the find match was not replaced"
if grep -qF "line 042" "$SAMPLE"; then
  fail "the original find target is still present after replacement"
fi

step "the open dialog opens a second tab"
OPENED=0
for attempt in 1 2 3; do
  key ctrl+o
  sleep 1
  key ctrl+a
  type_text "$SECOND"
  sleep 0.3
  key Return
  if wait_for_title_contains "second.txt" 5; then
    OPENED=1
    break
  fi
  key Escape
  sleep 0.3
done
[ "$OPENED" = "1" ] || fail "the built-in open dialog did not open second.txt"

step "Ctrl+Tab switches tabs"
key ctrl+Tab
wait_for_title_contains "sample.txt" 10 || fail "Ctrl+Tab did not return to sample.txt"
key ctrl+Tab
wait_for_title_contains "second.txt" 10 || fail "Ctrl+Tab did not switch back to second.txt"

step "Ctrl+W on a dirty tab keeps it open until cancelled"
editor_click
type_text "DIRTY"
sleep 0.4
wait_for_title_contains "second.txt *" 10 || fail "typing did not mark second.txt dirty"
key ctrl+w
sleep 0.8
if ! kill -0 "$APP_PID" 2>/dev/null; then
  fail "the application exited when closing a dirty tab"
fi
wait_for_title_contains "second.txt *" 5 || fail "the dirty tab closed without confirmation"
key Escape
sleep 0.5
if ! kill -0 "$APP_PID" 2>/dev/null; then
  fail "the application exited after cancelling the close prompt"
fi
wait_for_title_contains "second.txt" 5 || fail "the tab disappeared after cancelling the prompt"
# The dialog is gone: typing must reach the editor again.
type_text "!"
sleep 0.3
key ctrl+s
wait_for_file_contains "$SECOND" "DIRTY!" 10 || fail "saving after cancelling the prompt failed"

step "closing the saved tab returns to the remaining tab"
key ctrl+w
wait_for_title_contains "sample.txt" 10 || fail "closing the saved tab did not return to sample.txt"
if ! kill -0 "$APP_PID" 2>/dev/null; then
  fail "the application exited after the last tab operation"
fi

echo "gui-integration: ok"
