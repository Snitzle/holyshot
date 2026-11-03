#!/bin/bash
# A/B benchmark inside a hidden virtual KWin (nothing appears on screen).
#   bench/run.sh RUNS BINARY [BINARY...]
# Runs are interleaved across binaries so background load affects all alike.
# Prints medians per binary: time to first frame, peak RSS, CPU time.
set -u
RUNS=$1; shift
BINS=""; for b in "$@"; do BINS="$BINS $(readlink -f "$b")"; done
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d)
mkdir -p "$WORK/cfg/holyshot" "$WORK/cfg/gtk-4.0" "$WORK/shots"
printf '[holyshot]\nsave_dir=%s\ncopy_to_clipboard=false\nthumbnail_seconds=1\n' "$WORK/shots" > "$WORK/cfg/holyshot/config.ini"
cp "$HOME/.config/gtk-4.0/settings.ini" "$WORK/cfg/gtk-4.0/" 2>/dev/null
[ -x "$HERE/icount" ] || /usr/bin/gcc -O2 "$HERE/icount.c" -o "$HERE/icount"
# The editor flow needs a realistic screenshot. None is shipped (it would be a
# picture of someone's desktop); grab one of yours, locally, if missing.
if [ ! -s "$HERE/sample.png" ]; then
    "$HOME/.local/bin/holyshot" __dump "$HERE/sample.png" || { echo "need bench/sample.png"; exit 1; }
fi
export BINS RUNS WORK SAMPLE="$HERE/sample.png" ICOUNT="$HERE/icount"
setsid -f bash -c "timeout 1200 dbus-run-session -- '$HERE/inner.sh' > '$WORK/out' 2> '$WORK/err' < /dev/null"
while [ ! -e "$WORK/done" ]; do sleep 0.5; done
cat "$WORK/out"
rm -rf "$WORK"
