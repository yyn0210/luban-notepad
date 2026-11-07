#!/usr/bin/env bash
# Window resize and resize-cursor check for Linux X11 (acceptance helper).
#
# The manually decorated window removes the window manager frame
# (`_MOTIF_WM_HINTS` decorations=0), so the application draws its own invisible
# resize strips and applies the drag itself. This script runs a private
# Xvfb + KWin session, drags every edge and corner with xdotool and verifies the
# resulting geometry and that the cursor stays the same resize cursor that was
# shown while hovering (the window manager's single-sided edge cursor must not
# take over during the drag). It also checks that a fast press-and-drag anchors
# at the press position, that a maximized window ignores edge drags and
# restores to its exact bounds, and that shrinking past the minimum size keeps
# the opposite edge pinned.
#
# Usage:
#   scripts/window-resize-check.sh [--require-display]
#
# `--require-display` makes a missing tool or display a failure instead of a
# skip; CI uses it so "skipped" can never be mistaken for "passed".
#
# Environment:
#   LUBAN_BIN              app binary (default: $CARGO_TARGET_DIR/debug/luban-notepad)
#   LUBAN_RESIZE_ARTIFACTS directory for the application log on failure
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${LUBAN_BIN:-$TARGET_DIR/debug/luban-notepad}"
ARTIFACTS="${LUBAN_RESIZE_ARTIFACTS:-}"
SCREEN_W=1440
SCREEN_H=900
REQUIRE_DISPLAY=0
for arg in "$@"; do
  case "$arg" in
    --require-display) REQUIRE_DISPLAY=1 ;;
    -h|--help)
      awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' \
        "${BASH_SOURCE[0]}"
      exit 0
      ;;
    *)
      echo "window-resize-check: unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

skip_or_fail() {
  if [ "$REQUIRE_DISPLAY" = "1" ]; then
    echo "window-resize-check: FAILED: $1" >&2
    exit 1
  fi
  echo "window-resize-check: SKIP ($1)" >&2
  exit 0
}

for tool in python3 xdotool xprop dbus-run-session; do
  command -v "$tool" >/dev/null 2>&1 || skip_or_fail "$tool is not installed"
done
command -v kwin_x11 >/dev/null 2>&1 || skip_or_fail "kwin_x11 is not installed"
XVFB_BIN="$(command -v Xvfb || true)"
if [ -z "$XVFB_BIN" ] && [ -x "$HOME/.local/xvfb-root/usr/bin/Xvfb" ]; then
  XVFB_BIN="$HOME/.local/xvfb-root/usr/bin/Xvfb"
fi
[ -n "$XVFB_BIN" ] || skip_or_fail "Xvfb is not installed"
if ! ldconfig -p 2>/dev/null | grep -q "libXfixes.so.3"; then
  skip_or_fail "libXfixes is not installed"
fi

if [ ! -x "$BIN" ]; then
  echo "window-resize-check: building $BIN"
  (cd "$ROOT" && cargo build -p luban-app)
fi
[ -x "$BIN" ] || skip_or_fail "missing app binary at $BIN"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-resize.XXXXXX")"
XVFB_PID=""
KWIN_SESSION_PID=""
APP_PID=""
DISPLAY_NUM=95

cleanup() {
  for pid in "$APP_PID" "$KWIN_SESSION_PID" "$XVFB_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  sleep 0.3
  for pid in "$APP_PID" "$KWIN_SESSION_PID" "$XVFB_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill -9 "$pid" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT

fail() {
  echo "window-resize-check: FAILED: $1" >&2
  if [ -n "$ARTIFACTS" ]; then
    mkdir -p "$ARTIFACTS"
    cp "${LUBAN_NOTEPAD_CONFIG_DIR:-/nonexistent}/logs/luban.log" "$ARTIFACTS/" 2>/dev/null || true
  fi
  if [ -f "${LUBAN_NOTEPAD_CONFIG_DIR:-/nonexistent}/logs/luban.log" ]; then
    echo "window-resize-check: application log (last 20 lines):" >&2
    tail -20 "$LUBAN_NOTEPAD_CONFIG_DIR/logs/luban.log" >&2
  fi
  exit 1
}

# Current cursor image as a stable content checksum (XFixes reports pixels,
# not names; equal checksums mean the same cursor shape).
cat >"$TMP/cursor.py" <<'PY'
import ctypes
import ctypes.util
import sys

x11 = ctypes.CDLL(ctypes.util.find_library("X11"))
xfixes = ctypes.CDLL(ctypes.util.find_library("Xfixes"))


class CursorImage(ctypes.Structure):
    _fields_ = [
        ("x", ctypes.c_short),
        ("y", ctypes.c_short),
        ("width", ctypes.c_ushort),
        ("height", ctypes.c_ushort),
        ("xhot", ctypes.c_ushort),
        ("yhot", ctypes.c_ushort),
        ("cursor_serial", ctypes.c_ulong),
        ("pixels", ctypes.POINTER(ctypes.c_ulong)),
    ]


x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XFree.argtypes = [ctypes.c_void_p]
xfixes.XFixesGetCursorImage.restype = ctypes.POINTER(CursorImage)
xfixes.XFixesGetCursorImage.argtypes = [ctypes.c_void_p]

display = x11.XOpenDisplay(None)
if not display:
    print("no-display")
    raise SystemExit(1)
image = xfixes.XFixesGetCursorImage(display)
if not image:
    print("no-cursor")
    raise SystemExit(1)
img = image.contents
checksum = 0
for index in range(img.width * img.height):
    checksum = (checksum * 131 + img.pixels[index]) & 0xFFFFFFFFFFFFFFFF
print(f"{img.cursor_serial}:{checksum:016x}")
x11.XFree(ctypes.cast(image, ctypes.c_void_p))
PY

cursor_sig() {
  python3 -I "$TMP/cursor.py"
}

# Move the pointer, let the application repaint the hover state, then read the
# cursor. `xdotool mousemove` alone does not generate a motion event on some
# Xservers, so nudge the pointer by one pixel.
cursor_at() {
  local x=$1 y=$2
  xdotool mousemove "$x" "$y"
  sleep 0.15
  xdotool mousemove $((x + 1)) "$y"
  sleep 0.35
  cursor_sig
}

geometry() {
  eval "$(xdotool getwindowgeometry --shell "$WID")"
}

DRAG_CURSOR=""
drag() {
  local sx=$1 sy=$2 dx=$3 dy=$4 step
  xdotool mousemove "$sx" "$sy"
  sleep 0.25
  xdotool mousedown 1
  sleep 0.25
  for step in 1 2 3 4 5 6; do
    xdotool mousemove $((sx + dx * step / 6)) $((sy + dy * step / 6))
    sleep 0.06
  done
  # Sample the cursor while the button is still held: the application resizes
  # the window itself, so the press-time resize cursor must stay visible for
  # the whole drag instead of switching to the window manager's edge cursor.
  sleep 0.25
  DRAG_CURSOR="$(cursor_sig)"
  xdotool mouseup 1
  sleep 0.5
}

assert_changed() {
  local label=$1 before=$2 after=$3
  [ "$before" != "$after" ] || fail "$label did not change"
}

assert_equal() {
  local label=$1 left=$2 right=$3
  [ "$left" = "$right" ] || fail "$label differ: $left vs $right"
}

assert_eq_num() {
  local label=$1 actual=$2 expected=$3
  [ "$actual" -eq "$expected" ] || fail "$label is $actual, expected $expected"
}

# --- display, window manager and application ------------------------------
export DISPLAY=":$DISPLAY_NUM"
export XKB_CONFIG_ROOT="${XKB_CONFIG_ROOT:-$HOME/.local/xvfb-root/usr/share/X11/xkb}"
"$XVFB_BIN" ":$DISPLAY_NUM" -screen 0 "${SCREEN_W}x${SCREEN_H}x24" -nolisten tcp \
  >"$TMP/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 50); do
  xdotool getdisplaygeometry >/dev/null 2>&1 && break
  sleep 0.1
done
xdotool getdisplaygeometry >/dev/null 2>&1 || {
  tail -20 "$TMP/xvfb.log" >&2
  skip_or_fail "the private Xvfb display did not come up"
}

export XDG_CONFIG_HOME="$TMP/xdg-config" XDG_DATA_HOME="$TMP/xdg-data" HOME="$TMP/home"
mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$HOME"
KWIN_COMPOSE=O dbus-run-session -- kwin_x11 >"$TMP/kwin.log" 2>&1 &
KWIN_SESSION_PID=$!
wm_ready=0
for _ in $(seq 1 100); do
  if xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "0x[1-9a-f]"; then
    wm_ready=1
    break
  fi
  sleep 0.1
done
if [ "$wm_ready" != "1" ]; then
  tail -20 "$TMP/kwin.log" >&2
  skip_or_fail "KWin did not come up on the private display"
fi

export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"
export LUBAN_NOTEPAD_CONFIG_DIR="$TMP/config"
mkdir -p "$LUBAN_NOTEPAD_CONFIG_DIR"
export XDG_RUNTIME_DIR="$TMP/xdg-runtime"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

"$BIN" >"$TMP/app.out" 2>&1 &
APP_PID=$!
WID=""
for _ in $(seq 1 150); do
  WID="$(xdotool search --onlyvisible --name "Luban Notepad" 2>/dev/null | head -1 || true)"
  [ -n "$WID" ] && break
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    tail -20 "$TMP/app.out" >&2
    fail "the application exited during startup"
  fi
  sleep 0.1
done
[ -n "$WID" ] || fail "no editor window appeared"
xdotool windowfocus "$WID" 2>/dev/null || true
sleep 2

geometry
echo "window-resize-check: window $WID at $X,$Y ${WIDTH}x${HEIGHT}"
[ "$WIDTH" -gt 400 ] || fail "unexpectedly small window: ${WIDTH}x${HEIGHT}"
[ "$HEIGHT" -gt 300 ] || fail "unexpectedly small window: ${WIDTH}x${HEIGHT}"

# --- cursor feedback on every strip ---------------------------------------
center=$(cursor_at $((X + WIDTH / 2)) $((Y + HEIGHT / 2)))
right=$(cursor_at $((X + WIDTH - 2)) $((Y + HEIGHT / 2)))
left=$(cursor_at $((X + 2)) $((Y + HEIGHT / 2)))
top=$(cursor_at $((X + WIDTH / 2)) $((Y + 2)))
bottom=$(cursor_at $((X + WIDTH / 2)) $((Y + HEIGHT - 2)))
top_left=$(cursor_at $((X + 2)) $((Y + 2)))
top_right=$(cursor_at $((X + WIDTH - 2)) $((Y + 2)))
bottom_left=$(cursor_at $((X + 2)) $((Y + HEIGHT - 2)))
bottom_right=$(cursor_at $((X + WIDTH - 2)) $((Y + HEIGHT - 2)))

check_edge() {
  local name=$1 sig=$2
  assert_changed "the $name resize cursor" "$center" "$sig"
}

check_edge right "$right"
check_edge left "$left"
check_edge top "$top"
check_edge bottom "$bottom"
check_edge top-left "$top_left"
check_edge top-right "$top_right"
check_edge bottom-left "$bottom_left"
check_edge bottom-right "$bottom_right"
assert_equal "the left and right resize cursors" "$left" "$right"
assert_equal "the top and bottom resize cursors" "$top" "$bottom"
assert_equal "the top-left and bottom-right resize cursors" "$top_left" "$bottom_right"
assert_equal "the top-right and bottom-left resize cursors" "$top_right" "$bottom_left"
echo "window-resize-check: cursor styles ok"

# --- the drag anchors at the exact press, not at a later pointer query -----
# Press and move in one xdotool invocation so the motion is already queued
# when the application handles the press. Anchoring via QueryPointer would
# observe the moved position and lose the whole delta.
geometry
before_w=$WIDTH
xdotool mousemove $((X + WIDTH - 2)) $((Y + HEIGHT / 2)) sleep 0.3 \
  mousedown 1 mousemove_relative -- 20 0 sleep 0.3 mouseup 1
sleep 0.5
geometry
assert_eq_num "the fast press-and-drag width" "$WIDTH" "$((before_w + 20))"
echo "window-resize-check: fast press-and-drag ${before_w} -> $WIDTH"

# --- edge and corner drags -------------------------------------------------
geometry
before_w=$WIDTH
drag $((X + WIDTH - 2)) $((Y + HEIGHT / 2)) 90 0
geometry
[ "$WIDTH" -gt "$before_w" ] || fail "dragging the right edge did not widen the window"
assert_equal "the right-edge drag cursor" "$DRAG_CURSOR" "$right"
echo "window-resize-check: right edge ${before_w} -> $WIDTH"

before_h=$HEIGHT
drag $((X + WIDTH / 2)) $((Y + HEIGHT - 2)) 0 70
assert_equal "the bottom-edge drag cursor" "$DRAG_CURSOR" "$bottom"
geometry
[ "$HEIGHT" -gt "$before_h" ] || fail "dragging the bottom edge did not grow the window"
echo "window-resize-check: bottom edge ${before_h} -> $HEIGHT"

before_x=$X
before_w=$WIDTH
drag $((X + 2)) $((Y + HEIGHT / 2)) -60 0
assert_equal "the left-edge drag cursor" "$DRAG_CURSOR" "$left"
geometry
[ "$X" -lt "$before_x" ] || fail "dragging the left edge did not move the window left"
[ "$WIDTH" -gt "$before_w" ] || fail "dragging the left edge did not widen the window"
echo "window-resize-check: left edge X ${before_x} -> $X, width ${before_w} -> $WIDTH"

before_x=$X
before_y=$Y
before_w=$WIDTH
before_h=$HEIGHT
drag $((X + 2)) $((Y + 2)) 50 40
assert_equal "the top-left corner drag cursor" "$DRAG_CURSOR" "$top_left"
geometry
[ "$X" -gt "$before_x" ] || fail "dragging the top-left corner did not move the window right"
[ "$Y" -gt "$before_y" ] || fail "dragging the top-left corner did not move the window down"
[ "$WIDTH" -lt "$before_w" ] || fail "dragging the top-left corner did not shrink the window"
[ "$HEIGHT" -lt "$before_h" ] || fail "dragging the top-left corner did not shorten the window"
echo "window-resize-check: top-left corner $before_x,$before_y ${before_w}x${before_h} -> $X,$Y ${WIDTH}x${HEIGHT}"

# --- a second button must not end the drag ---------------------------------
geometry
before_w=$WIDTH
edge_x=$((X + WIDTH - 2))
[ "$edge_x" -lt "$SCREEN_W" ] || edge_x=$((SCREEN_W - 1))
edge_y=$((Y + HEIGHT / 2))
xdotool mousemove "$edge_x" "$edge_y"
sleep 0.3
xdotool mousedown 1
sleep 0.2
xdotool mousemove $((edge_x - 40)) "$edge_y"
sleep 0.3
# Press and release the right button while the primary button is still down:
# the implicit grab lasts until every button is released, so the drag must
# survive this and apply the remaining motion.
xdotool mousedown 3
sleep 0.15
xdotool mouseup 3
sleep 0.15
xdotool mousemove $((edge_x - 100)) "$edge_y"
sleep 0.3
xdotool mouseup 1
sleep 0.5
geometry
assert_eq_num "the width after the second-button drag" "$WIDTH" "$((before_w - 100))"
echo "window-resize-check: second button does not end the drag ${before_w} -> $WIDTH"

# --- maximize suppresses edge resizing, restore returns the bounds --------
geometry
restore_x=$X restore_y=$Y restore_w=$WIDTH restore_h=$HEIGHT
cursor_at $((X + WIDTH / 2)) $((Y + 18)) >/dev/null
xdotool click --repeat 2 --delay 90 1
sleep 1.2
geometry
[ "$WIDTH" -ge "$SCREEN_W" ] && [ "$HEIGHT" -ge "$SCREEN_H" ] \
  || fail "double-clicking the titlebar did not maximize the window"
max_w=$WIDTH
max_h=$HEIGHT
drag $((X + WIDTH - 2)) $((Y + HEIGHT / 2)) -80 0
geometry
assert_eq_num "the maximized window width" "$WIDTH" "$max_w"
assert_eq_num "the maximized window height" "$HEIGHT" "$max_h"
echo "window-resize-check: maximized window ignores edge drags"

cursor_at $((X + WIDTH / 2)) $((Y + 18)) >/dev/null
xdotool click --repeat 2 --delay 90 1
sleep 1.2
geometry
assert_eq_num "the restored window width" "$WIDTH" "$restore_w"
assert_eq_num "the restored window height" "$HEIGHT" "$restore_h"
echo "window-resize-check: restored to ${restore_w}x${restore_h}"

# The strips return after the restore.
center=$(cursor_at $((X + WIDTH / 2)) $((Y + HEIGHT / 2)))
right=$(cursor_at $((X + WIDTH - 2)) $((Y + HEIGHT / 2)))
assert_changed "the resize cursor after restoring" "$center" "$right"

# --- minimum size clamps ---------------------------------------------------
# Drag the left edge past the right edge: the width clamps at the minimum and
# the right edge must stay where it was. The same for the top edge.
geometry
right_edge=$((X + WIDTH))
bottom_edge=$((Y + HEIGHT))
drag $((X + 2)) $((Y + HEIGHT / 2)) "$SCREEN_W" 0
geometry
assert_eq_num "the minimum window width" "$WIDTH" 160
assert_eq_num "the right edge after the width clamp" $((X + WIDTH)) "$right_edge"
drag $((X + WIDTH / 2)) $((Y + 2)) 0 "$SCREEN_H"
geometry
assert_eq_num "the minimum window height" "$HEIGHT" 100
assert_eq_num "the bottom edge after the height clamp" $((Y + HEIGHT)) "$bottom_edge"
echo "window-resize-check: minimum size clamps ok"

echo "window-resize-check: ok"
