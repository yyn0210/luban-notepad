#!/usr/bin/env bash
# Native-titlebar flash check for Linux X11 (acceptance helper A1/A2).
#
# Starts a private Xvfb server with KWin as the window manager and no
# compositor, records the screen at 120 fps with ffmpeg while the application
# launches (A1: repeated launches; A2: a new window opened over IPC) and
# analyses the recording for frames in which the window manager drew a native
# titlebar. The detector finds the window's top edge adaptively (KWin places
# windows at varying positions) and classifies the titlebar band by its text
# layout: a native titlebar has centered window-title text and an empty left
# side, the application-drawn titlebar has text starting at the left edge.
#
# Per acceptance A1 the check window is the first 45 frames (120 fps ≈ 375 ms)
# after the window first appears; any native-titlebar frame in that window
# fails the run.
#
# Usage:
#   scripts/titlebar-flash-check.sh [--launches N] [--new-window]
#                                   [--require-display]
#
#   --launches N      A1: number of consecutive app launches (default 10)
#   --new-window      A2: run one launch, then open a second window over IPC
#                     via `luban-notepad --new-window` and check its first
#                     frames instead of the initial launch
#   --require-display make a missing tool or display a failure instead of a
#                     skip
#
# Environment:
#   LUBAN_BIN            app binary (default: $CARGO_TARGET_DIR/debug/luban-notepad)
#   LUBAN_FLASH_ARTIFACTS directory for recordings and analysis output
#   LUBAN_FLASH_DISPLAY  reuse this display; a KWin session must already run
#                        there (Xvfb/KWin are started automatically otherwise)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
BIN="${LUBAN_BIN:-$TARGET_DIR/debug/luban-notepad}"
ARTIFACTS="${LUBAN_FLASH_ARTIFACTS:-}"
SCREEN_W=1440
SCREEN_H=900
FPS=120
CHECK_FRAMES=45
LAUNCHES=10
MODE=a1
REQUIRE_DISPLAY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --launches)
      [ $# -ge 2 ] || { echo "titlebar-flash-check: --launches needs a number" >&2; exit 2; }
      LAUNCHES="$2"; shift 2 ;;
    --launches=*) LAUNCHES="${1#--launches=}"; shift ;;
    --new-window) MODE=a2; shift ;;
    --require-display) REQUIRE_DISPLAY=1; shift ;;
    -h|--help)
      sed -n '2,32p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "titlebar-flash-check: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

skip_or_fail() {
  if [ "$REQUIRE_DISPLAY" = "1" ]; then
    echo "titlebar-flash-check: FAILED: $1" >&2
    exit 1
  fi
  echo "titlebar-flash-check: SKIP ($1)" >&2
  exit 0
}

for tool in ffmpeg python3 xdotool dbus-run-session; do
  command -v "$tool" >/dev/null 2>&1 || skip_or_fail "$tool is not installed"
done
command -v kwin_x11 >/dev/null 2>&1 || skip_or_fail "kwin_x11 is not installed"

if [ ! -x "$BIN" ]; then
  echo "titlebar-flash-check: building $BIN"
  (cd "$ROOT" && cargo build -p luban-app)
fi
[ -x "$BIN" ] || skip_or_fail "missing app binary at $BIN"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-flash.XXXXXX")"
XVFB_PID=""
KWIN_SESSION_PID=""
FFMPEG_PID=""
APP_PID=""

cleanup() {
  for pid in "$APP_PID" "$FFMPEG_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  sleep 0.3
  for pid in "$APP_PID" "$FFMPEG_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill -9 "$pid" 2>/dev/null || true
    fi
  done
  if [ -n "$KWIN_SESSION_PID" ] && kill -0 "$KWIN_SESSION_PID" 2>/dev/null; then
    kill "$KWIN_SESSION_PID" 2>/dev/null || true
  fi
  if [ -n "$XVFB_PID" ] && kill -0 "$XVFB_PID" 2>/dev/null; then
    kill "$XVFB_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

start_recording() {
  local out="$1"
  ffmpeg -loglevel error -nostdin \
    -f x11grab -framerate "$FPS" -video_size "${SCREEN_W}x${SCREEN_H}" -i "${DISPLAY}.0" \
    -c:v libx264 -preset ultrafast -crf 12 -pix_fmt yuv420p \
    -fps_mode cfr -r "$FPS" -y "$out" >/dev/null 2>&1 &
  FFMPEG_PID=$!
  sleep 0.4 # let the grabber produce its first frames before the launch
}

stop_recording() {
  local out="$1"
  if [ -n "$FFMPEG_PID" ] && kill -0 "$FFMPEG_PID" 2>/dev/null; then
    kill -INT "$FFMPEG_PID" 2>/dev/null || true
    for _ in $(seq 1 50); do
      kill -0 "$FFMPEG_PID" 2>/dev/null || break
      sleep 0.1
    done
  fi
  FFMPEG_PID=""
  [ -s "$out" ] || return 1
}

wait_exit() {
  local pid="$1"
  for _ in $(seq 1 50); do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.1
  done
  kill -9 "$pid" 2>/dev/null || true
  return 1
}

# Analyze a recording; prints one result line and fails on native frames.
#   analyze <recording> <mode-tag> [crop_x,crop_y,crop_w,crop_h]
analyze() {
  local rec="$1" tag="$2" crop="${3:-}"
  local crop_args=()
  if [ -n "$crop" ]; then
    crop_args=(--crop "$crop")
  fi
  if [ -n "$ARTIFACTS" ]; then
    mkdir -p "$ARTIFACTS"
  fi
  python3 - "$rec" "$FPS" "$CHECK_FRAMES" "$tag" "${crop_args[@]:-}" <<'PY'
import subprocess, sys

rec, fps, check_frames, tag = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
crop = None
if len(sys.argv) > 5 and sys.argv[5] == "--crop":
    crop = tuple(int(v) for v in sys.argv[6].split(","))

vf = "format=gray"
if crop:
    vf = f"crop={crop[2]}:{crop[3]}:{crop[0]}:{crop[1]},{vf}"
    # Frame size must match the cropped pipe, not the recording.
    w, h = crop[2], crop[3]
else:
    first = subprocess.check_output(
        ["ffprobe", "-v", "error", "-select_streams", "v:0",
         "-show_entries", "stream=width,height", "-of", "csv=p=0", rec]
    ).decode().strip().split(",")
    w, h = int(first[0]), int(first[1])
frame_size = w * h
proc = subprocess.Popen(
    ["ffmpeg", "-loglevel", "error", "-i", rec, "-vf", vf,
     "-f", "rawvideo", "pipe:1"],
    stdout=subprocess.PIPE)

ROW_BG = 15          # a row brighter than this is window, not desktop
LEFT_MAX, CENTER_MIN = 0.01, 0.005  # text-pixel fraction thresholds

def text_frac(px):
    # Fraction of pixels that differ strongly from the region median, i.e.
    # text or icon pixels on an otherwise uniform titlebar band.
    med = sorted(px)[len(px) // 2]
    return sum(1 for v in px if abs(v - med) > 60) / len(px)

def classify(frame, w, h):
    rows = [sum(frame[y*w:(y+1)*w]) / w for y in range(h)]
    y0 = next((y for y, m in enumerate(rows) if m > ROW_BG), None)
    if y0 is None:
        return None
    # The titlebar band ends where the bright rows end (a native-titlebar
    # frame has black, not-yet-painted content below it); cap it for windows
    # that are bright all the way down.
    y1 = y0
    while y1 < min(y0 + 44, h - 1) and rows[y1 + 1] > ROW_BG:
        y1 += 1
    band_rows = range(y0, y1 + 1)
    cols = [sum(frame[y*w+x] for y in band_rows) / len(band_rows)
            for x in range(w)]
    live = [x for x, m in enumerate(cols) if m > ROW_BG]
    if not live:
        return None
    x0, x1 = live[0], live[-1]
    if x1 - x0 < 200:
        return None
    mid = (x0 + x1) // 2
    span = x1 - x0
    l_end = min(x0 + max(40, int(span * 0.20)), mid)
    left = [frame[y*w+x] for y in band_rows for x in range(x0 + 8, l_end, 2)]
    c_lo, c_hi = max(x0, mid - int(span * 0.18)), min(x1, mid + int(span * 0.18))
    center = [frame[y*w+x] for y in band_rows for x in range(c_lo, c_hi, 2)]
    if not left or not center:
        return None
    # Native titlebar: empty on the left, window-title text in the center.
    if text_frac(left) < LEFT_MAX and text_frac(center) > CENTER_MIN:
        return "native"
    return "app"

appearance = None
native_in_window = 0
frame_idx = 0
buf = proc.stdout.read(frame_size)
while len(buf) == frame_size:
    kind = classify(buf, w, h)
    if appearance is None:
        if kind is not None:
            appearance = frame_idx
    elif frame_idx < appearance + check_frames and kind == "native":
        native_in_window += 1
    if appearance is not None and frame_idx >= appearance + check_frames - 1:
        break
    frame_idx += 1
    buf = proc.stdout.read(frame_size)
proc.stdout.close()
proc.wait()

if appearance is None:
    print(f"titlebar-flash-check: {tag}: no window appeared in the recording")
    sys.exit(3)
verdict = "PASS" if native_in_window == 0 else "FAIL"
print(f"titlebar-flash-check: {tag}: window first visible at frame {appearance} "
      f"({appearance / fps:.2f}s); native-titlebar frames in the following "
      f"{check_frames}: {native_in_window} -> {verdict}")
sys.exit(0 if native_in_window == 0 else 1)
PY
}

# --- display and window manager setup -------------------------------------
if [ -n "${LUBAN_FLASH_DISPLAY:-}" ]; then
  export DISPLAY="$LUBAN_FLASH_DISPLAY"
else
  XVFB_BIN="$(command -v Xvfb || true)"
  if [ -z "$XVFB_BIN" ] && [ -x "$HOME/.local/xvfb-root/usr/bin/Xvfb" ]; then
    XVFB_BIN="$HOME/.local/xvfb-root/usr/bin/Xvfb"
    if [ -d "$HOME/.local/xvfb-root/usr/share/X11/xkb" ]; then
      export XKB_CONFIG_ROOT="${XKB_CONFIG_ROOT:-$HOME/.local/xvfb-root/usr/share/X11/xkb}"
    fi
  fi
  [ -n "$XVFB_BIN" ] || skip_or_fail "no Xvfb available"
  "$XVFB_BIN" :98 -screen 0 "${SCREEN_W}x${SCREEN_H}x24" -nolisten tcp \
    >"$TMP/xvfb.log" 2>&1 &
  XVFB_PID=$!
  export DISPLAY=:98
  for _ in $(seq 1 50); do
    xdotool getdisplaygeometry >/dev/null 2>&1 && break
    sleep 0.1
  done
  xdotool getdisplaygeometry >/dev/null 2>&1 || {
    tail -20 "$TMP/xvfb.log" >&2
    skip_or_fail "the private Xvfb display did not come up"
  }

  # KWin as window manager, compositing disabled, isolated configuration so
  # the user's desktop settings cannot leak in. No compositor ever runs, which
  # is exactly the environment where the native titlebar used to flash.
  export XDG_CONFIG_HOME="$TMP/xdg-config" XDG_DATA_HOME="$TMP/xdg-data"
  export HOME="$TMP/home"
  mkdir -p "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$HOME"
  KWIN_COMPOSE=O dbus-run-session -- kwin_x11 >"$TMP/kwin.log" 2>&1 &
  KWIN_SESSION_PID=$!
  wm_ready=0
  for _ in $(seq 1 100); do
    if command -v xprop >/dev/null 2>&1; then
      if xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "0x[1-9a-f]"; then
        wm_ready=1; break
      fi
    fi
    sleep 0.1
  done
  if [ "$wm_ready" != "1" ] && command -v xprop >/dev/null 2>&1; then
    tail -20 "$TMP/kwin.log" >&2
    skip_or_fail "KWin did not come up on the private display"
  fi
  [ "$wm_ready" = "1" ] || sleep 5 # no xprop: give KWin a fixed settle time
fi

export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"
export LUBAN_NOTEPAD_CONFIG_DIR="$TMP/config"
mkdir -p "$LUBAN_NOTEPAD_CONFIG_DIR"
export XDG_RUNTIME_DIR="$TMP/xdg-runtime"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

fail() {
  echo "titlebar-flash-check: FAILED: $1" >&2
  if [ -f "$LUBAN_NOTEPAD_CONFIG_DIR/logs/luban.log" ]; then
    echo "titlebar-flash-check: application log (last 30 lines):" >&2
    tail -30 "$LUBAN_NOTEPAD_CONFIG_DIR/logs/luban.log" >&2
  fi
  exit 1
}

if [ "$MODE" = "a1" ]; then
  echo "titlebar-flash-check: A1: $LAUNCHES consecutive launches"
  for i in $(seq 1 "$LAUNCHES"); do
    rec="$TMP/launch-$i.mkv"
    start_recording "$rec" || fail "ffmpeg could not record the display"
    "$BIN" >"$TMP/app-$i.out" 2>&1 &
    APP_PID=$!
    sleep 3
    stop_recording "$rec" || fail "launch $i produced no recording"
    wait_exit "$APP_PID" || true
    APP_PID=""
    if ! analyze "$rec" "launch-$i"; then
      [ -n "$ARTIFACTS" ] && cp "$rec" "$ARTIFACTS/" 2>/dev/null || true
      exit 1
    fi
    rm -f "$rec"
  done
  echo "titlebar-flash-check: ok"
  exit 0
fi

# A2: one running instance, a second window opened over IPC.
echo "titlebar-flash-check: A2: new window via IPC --new-window"
"$BIN" >"$TMP/app-first.out" 2>&1 &
APP_PID=$!
W1=""
for _ in $(seq 1 100); do
  W1="$(xdotool search --onlyvisible --name "Luban Notepad" 2>/dev/null | head -1 || true)"
  [ -n "$W1" ] && break
  kill -0 "$APP_PID" 2>/dev/null || fail "the first instance exited during startup"
  sleep 0.1
done
[ -n "$W1" ] || fail "no editor window appeared"
# Park the first window in the bottom-right corner so KWin's placement of the
# new window cannot put another window's titlebar into the analysis band.
xdotool windowsize "$W1" 200 200 >/dev/null 2>&1 || true
xdotool windowmove "$W1" 1230 690 >/dev/null 2>&1 || true
sleep 0.5

rec="$TMP/new-window.mkv"
start_recording "$rec" || fail "ffmpeg could not record the display"
"$BIN" --new-window >"$TMP/app-second.out" 2>&1 &
sleep 3
W2=""
for _ in $(seq 1 20); do
  W2="$(xdotool search --onlyvisible --name "Luban Notepad" 2>/dev/null | grep -v "^$W1$" | head -1 || true)"
  [ -n "$W2" ] && break
  sleep 0.1
done
stop_recording "$rec" || fail "the recording is empty"
wait_exit "$APP_PID" || true
APP_PID=""
[ -n "$W2" ] || fail "no second window appeared after --new-window"
# Post-hoc geometry: the final window rectangle. The top margin covers the
# titlebar height that the frame had while the native titlebar was visible.
# Clamp to the screen: ffmpeg's crop filter rejects negative offsets.
eval "$(xdotool getwindowgeometry --shell "$W2")"
CX=$((X - 6)); [ "$CX" -lt 0 ] && CX=0
CY=$((Y - 50)); [ "$CY" -lt 0 ] && CY=0
CW=$((WIDTH + 12)); [ $((CX + CW)) -gt 1440 ] && CW=$((1440 - CX))
# Only the top band matters for the titlebar check; cap the crop height so a
# window parked in the corner cannot enter the analysis region.
CH=$((HEIGHT + 62)); [ "$CH" -gt 110 ] && CH=110
[ $((CY + CH)) -gt 900 ] && CH=$((900 - CY))
crop="$CX,$CY,$CW,$CH"
echo "titlebar-flash-check: new-window geometry: X=$X Y=$Y W=$WIDTH H=$HEIGHT crop=$crop"
if ! analyze "$rec" "new-window" "$crop"; then
  [ -n "$ARTIFACTS" ] && cp "$rec" "$ARTIFACTS/" 2>/dev/null || true
  exit 1
fi
echo "titlebar-flash-check: ok"
