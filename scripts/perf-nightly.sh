#!/usr/bin/env bash
# Nightly performance budgets.
#
# Measures opening a 200 MB file in the release GUI, then runs the disk-engine
# benchmark (`crates/luban-core/benches/disk_engine_perf.rs`), which covers
# 1 GB open, 1 GB full-text find, 1 GB save and 3 GB RSS growth. Every
# measurement is written as a JSON line; the script exits non-zero when a
# budget is exceeded.
#
# Usage:
#   scripts/perf-nightly.sh [--scale <ratio>] [--skip-app] [--self-test]
#
# --scale scales the generated file sizes and the engine budgets for quick
# local runs. The GUI open budget stays at its full value because process
# startup dominates a 200 MB open and does not scale with the file. --self-test
# runs a tiny measurement with an absurd budget factor and succeeds only when
# the failure path reports a budget violation.
#
# Environment:
#   LUBAN_BIN             app binary (default: $CARGO_TARGET_DIR/release/luban-notepad)
#   LUBAN_PERF_SCALE      same as --scale
#   LUBAN_PERF_BUDGET_FACTOR  multiplies every budget (default 1.0)
#   LUBAN_PERF_REPORT     write the JSON-line report to this path
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
SCALE="${LUBAN_PERF_SCALE:-1.0}"
FACTOR="${LUBAN_PERF_BUDGET_FACTOR:-1.0}"
SKIP_APP=0
SELF_TEST=0
ORIG_ARGS=("$@")

while [ $# -gt 0 ]; do
  case "$1" in
    --scale)
      SCALE="${2:?--scale needs a value}"
      shift 2
      ;;
    --scale=*) SCALE="${1#*=}"; shift ;;
    --skip-app) SKIP_APP=1; shift ;;
    --self-test) SELF_TEST=1; shift ;;
    -h|--help)
      sed -n '2,22p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "perf-nightly: unknown argument: $1" >&2
      exit 2
      ;;
  esac
done

if [ "$SELF_TEST" = "1" ]; then
  SCALE=0.02
  FACTOR=0.0001
fi

REPORT="${LUBAN_PERF_REPORT:-$ROOT/perf-report.json}"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/luban-perf.XXXXXX")"
APP_PID=""
FAILED=0

cleanup() {
  if [ -n "$APP_PID" ] && kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
    sleep 0.3
    kill -9 "$APP_PID" 2>/dev/null || true
  fi
  if [ -n "${LUBAN_PERF_XVFB_PID:-}" ] && kill -0 "$LUBAN_PERF_XVFB_PID" 2>/dev/null; then
    kill "$LUBAN_PERF_XVFB_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

record() {
  echo "$1" | tee -a "$REPORT"
}

rm -f "$REPORT"
record "{\"metric\":\"environment\",\"scale\":$SCALE,\"budget_factor\":$FACTOR,\"self_test\":$([ "$SELF_TEST" = "1" ] && echo true || echo false)}"

scaled_ms() {
  python3 - "$1" "$SCALE" <<'PY'
import sys
base, scale = float(sys.argv[1]), float(sys.argv[2])
print(max(1, round(base * scale)))
PY
}

app_bytes() {
  python3 - "$SCALE" <<'PY'
import sys
scale = float(sys.argv[1])
print(max(1, int(200 * 1024 * 1024 * scale)))
PY
}

if [ "$SKIP_APP" = "0" ]; then
  # A private display keeps the benchmark away from the user's desktop.
  if [ -n "${LUBAN_PERF_DISPLAY:-}" ]; then
    export DISPLAY="$LUBAN_PERF_DISPLAY"
  elif [ "${LUBAN_PERF_UNDER_XVFB:-0}" = "1" ] && xdotool getdisplaygeometry >/dev/null 2>&1; then
    :
  elif command -v xvfb-run >/dev/null 2>&1; then
    export LUBAN_PERF_UNDER_XVFB=1
    exec xvfb-run -a -s "-screen 0 1440x900x24" "$0" "${ORIG_ARGS[@]}"
  else
    XVFB_BIN="$(command -v Xvfb || true)"
    if [ -z "$XVFB_BIN" ] && [ -x "$HOME/.local/xvfb-root/usr/bin/Xvfb" ]; then
      XVFB_BIN="$HOME/.local/xvfb-root/usr/bin/Xvfb"
      export XKB_CONFIG_ROOT="${XKB_CONFIG_ROOT:-$HOME/.local/xvfb-root/usr/share/X11/xkb}"
    fi
    if [ -n "$XVFB_BIN" ]; then
      "$XVFB_BIN" :96 -screen 0 1440x900x24 -nolisten tcp >/dev/null 2>&1 &
      export LUBAN_PERF_XVFB_PID=$!
      export DISPLAY=:96
      for _ in $(seq 1 50); do
        xdotool getdisplaygeometry >/dev/null 2>&1 && break
        sleep 0.1
      done
    fi
  fi

  if ! command -v xdotool >/dev/null 2>&1 || ! xdotool getdisplaygeometry >/dev/null 2>&1; then
    echo "perf-nightly: no display for the app-open measurement" >&2
    FAILED=1
  else
    BIN="${LUBAN_BIN:-$TARGET_DIR/release/luban-notepad}"
    if [ -z "${LUBAN_BIN:-}" ]; then
      # Rebuild incrementally on every run so a stale binary from an earlier
      # revision can never be measured by accident.
      echo "perf-nightly: building the release app"
      (cd "$ROOT" && cargo build --release -p luban-app)
    fi
    if [ ! -x "$BIN" ]; then
      echo "perf-nightly: missing app binary at $BIN" >&2
      FAILED=1
    else
      BIG="$TMP/perf-big.txt"
      SIZE="$(app_bytes)"
      python3 - "$BIG" "$SIZE" <<'PY'
import sys
path, size = sys.argv[1], int(sys.argv[2])
line = b"line 00000000 some text to make each line long enough to index\n"
chunk = line * (1024 * 1024 // len(line) + 1)
chunk = chunk[: 1024 * 1024]
with open(path, "wb") as handle:
    written = 0
    while written < size:
        take = min(len(chunk), size - written)
        handle.write(chunk[:take])
        written += take
PY
      export WGPU_BACKEND="${WGPU_BACKEND:-vulkan}"
      export LIBGL_ALWAYS_SOFTWARE="${LIBGL_ALWAYS_SOFTWARE:-1}"
      export XDG_RUNTIME_DIR="$TMP/xdg"
      export LUBAN_NOTEPAD_CONFIG_DIR="$TMP/config"
      mkdir -p "$XDG_RUNTIME_DIR" "$LUBAN_NOTEPAD_CONFIG_DIR"
      chmod 700 "$XDG_RUNTIME_DIR"
      START_NS="$(date +%s%N)"
      "$BIN" "$BIG" >"$TMP/app.out" 2>&1 &
      APP_PID=$!
      FOUND=0
      for _ in $(seq 1 12000); do
        if xdotool search --onlyvisible --name "perf-big.txt - Luban Notepad" >/dev/null 2>&1; then
          FOUND=1
          break
        fi
        if ! kill -0 "$APP_PID" 2>/dev/null; then
          break
        fi
        sleep 0.02
      done
      END_NS="$(date +%s%N)"
      ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
      ROADMAP_MS=1500
      # GUI budget: roadmap target x 3.0 for software rendering, cold caches and
      # shared runners. Startup dominates, so --scale does not shrink it.
      BUDGET_MS="$(python3 - "$ROADMAP_MS" "$FACTOR" <<'PY'
import sys
roadmap, factor = (float(value) for value in sys.argv[1:3])
print(max(1, round(roadmap * 3.0 * factor)))
PY
)"
      if [ "$FOUND" != "1" ]; then
        record "{\"metric\":\"app_open_200mb\",\"size_bytes\":$SIZE,\"measured_ms\":$ELAPSED_MS,\"budget_ms\":$BUDGET_MS,\"passed\":false,\"note\":\"window title never appeared\"}"
        FAILED=1
      else
        PASSED=true
        if [ "$ELAPSED_MS" -gt "$BUDGET_MS" ]; then
          PASSED=false
          FAILED=1
        fi
        record "{\"metric\":\"app_open_200mb\",\"size_bytes\":$SIZE,\"measured_ms\":$ELAPSED_MS,\"budget_ms\":$BUDGET_MS,\"roadmap_ms\":$ROADMAP_MS,\"passed\":$PASSED}"
      fi
      kill "$APP_PID" 2>/dev/null || true
      APP_PID=""
      sleep 0.3
    fi
  fi
fi

BENCH_LOG="$TMP/bench.log"
set +e
(
  cd "$ROOT"
  LUBAN_PERF_SCALE="$SCALE" LUBAN_PERF_BUDGET_FACTOR="$FACTOR" \
    LUBAN_PERF_DIR="$TMP/work" cargo bench -p luban-core --bench disk_engine_perf
) 2>&1 | tee "$BENCH_LOG"
BENCH_STATUS=${PIPESTATUS[0]}
set -e
grep '^{"metric"' "$BENCH_LOG" >> "$REPORT" || true
if [ "$BENCH_STATUS" -ne 0 ]; then
  FAILED=1
fi

if [ "$SELF_TEST" = "1" ]; then
  if [ "$FAILED" = "1" ]; then
    echo "perf-nightly: self-test confirmed the budget failure path"
    exit 0
  fi
  echo "perf-nightly: self-test failed: budgets did not trip" >&2
  exit 1
fi

if [ "$FAILED" -ne 0 ]; then
  echo "perf-nightly: FAILED (report: $REPORT)" >&2
  exit 1
fi
echo "perf-nightly: ok (report: $REPORT)"
