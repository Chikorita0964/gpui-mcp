#!/usr/bin/env bash
# nextest run wrapper: one windowed test on a display of its own, Xvfb with openbox (whose EWMH
# window list native capture reads), so no window of another test takes focus from it mid-run and
# the refresh that follows never lands in a frame the test measures. Without Xvfb, openbox or xprop
# the test runs on the session's display as before.
set -euo pipefail

if ! command -v Xvfb >/dev/null || ! command -v openbox >/dev/null || ! command -v xprop >/dev/null; then
  exec "$@"
fi

dir=$(mktemp -d)
cleanup() {
  kill "${openbox_pid:-}" "${xvfb_pid:-}" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf -- "$dir"
}
trap cleanup EXIT

# A display number from 100 up, never the session's: Xvfb's lock file (/tmp/.X<n>-lock) reserves
# it, so a number another test holds makes this Xvfb exit and the next is tried. Only the abstract
# socket is used (a desktop such as WSLg may mount /tmp/.X11-unix read-only), and -displayfd
# reports the number once the server accepts connections. A display whose window manager cannot
# come up on it is stopped and another number tried; the warning keeps both logs, so a startup
# failure stays visible in the test's output.
start_display() {
  local n=$1
  : >"$dir/display"
  Xvfb ":$n" -displayfd 3 -screen 0 1920x1080x24 -nolisten tcp -nolisten unix \
    3>"$dir/display" 2>"$dir/xvfb.log" &
  xvfb_pid=$!
  while [ ! -s "$dir/display" ] && kill -0 "$xvfb_pid" 2>/dev/null; do sleep 0.05; done
  [ -s "$dir/display" ] || return 1
  # openbox marks the root window once it manages the screen
  DISPLAY=":$n" openbox --sm-disable >"$dir/openbox.log" 2>&1 &
  openbox_pid=$!
  until DISPLAY=":$n" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id'; do
    if ! kill -0 "$openbox_pid" 2>/dev/null; then
      local alive=no
      kill -0 "$xvfb_pid" 2>/dev/null && alive=yes
      {
        echo "own-display: openbox could not manage :$n (Xvfb running: $alive); trying another display"
        sed 's/^/  xvfb: /' "$dir/xvfb.log"
        sed 's/^/  openbox: /' "$dir/openbox.log"
      } >&2
      kill "$xvfb_pid" 2>/dev/null || true
      wait "$xvfb_pid" 2>/dev/null || true
      return 1
    fi
    sleep 0.05
  done
}

display=""
for _ in $(seq 20); do
  n=$((100 + RANDOM % 800))
  if start_display "$n"; then
    display=":$n"
    break
  fi
done
if [ -z "$display" ]; then
  cat "$dir/xvfb.log" >&2
  exit 1
fi
export DISPLAY="$display"
unset WAYLAND_DISPLAY

"$@"
