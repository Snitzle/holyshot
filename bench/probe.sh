#!/bin/bash
# Run one command in the hidden KWin with tracing; print its log.  bench/probe.sh ARGS...
HERE=$(cd "$(dirname "$0")" && pwd); WORK=$(mktemp -d)
mkdir -p "$WORK/cfg/holyshot" "$WORK/cfg/gtk-4.0" "$WORK/shots"
printf '[holyshot]\nsave_dir=%s\ncopy_to_clipboard=false\nthumbnail_seconds=1\n' "$WORK/shots" > "$WORK/cfg/holyshot/config.ini"
cp "$HOME/.config/gtk-4.0/settings.ini" "$WORK/cfg/gtk-4.0/"
printf '%q ' "$@" > "$WORK/cmd"
cat > "$WORK/in.sh" <<IN
SOCK=hs-probe-\$\$
KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 QT_QPA_PLATFORM=offscreen kwin_wayland --virtual --no-lockscreen --no-global-shortcuts --width 2992 --height 1253 --scale 1.15 --socket \$SOCK > /dev/null 2>&1 &
KW=\$!; for i in \$(seq 1 100); do [ -S "\$XDG_RUNTIME_DIR/\$SOCK" ] && break; sleep 0.1; done; sleep 1.5
export WAYLAND_DISPLAY=\$SOCK XDG_CONFIG_HOME=$WORK/cfg HOLYSHOT_TRACE=1 HOLYSHOT_BENCH_CLOSE_MS=300
$(cat "$WORK/cmd") > $WORK/log 2>&1
kill \$KW; wait \$KW 2>/dev/null; touch $WORK/done
IN
setsid -f bash -c "timeout 120 dbus-run-session -- bash '$WORK/in.sh' > /dev/null 2>&1 < /dev/null"
while [ ! -e "$WORK/done" ]; do sleep 0.3; done
cat "$WORK/log"; rm -rf "$WORK"
