#!/bin/bash
# Runs inside a private D-Bus session (see run.sh).
SOCK=hs-bench-$$
# Permission checks off in this throwaway compositor only, so any build can capture.
KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 QT_QPA_PLATFORM=offscreen kwin_wayland --virtual \
  --no-lockscreen --no-global-shortcuts --width 2992 --height 1253 --scale 1.15 --socket $SOCK > "$WORK/kwin.log" 2>&1 &
KW=$!
for i in $(seq 1 100); do [ -S "$XDG_RUNTIME_DIR/$SOCK" ] && break; sleep 0.1; done
sleep 1.5
export WAYLAND_DISPLAY=$SOCK XDG_CONFIG_HOME=$WORK/cfg HOLYSHOT_TRACE=1 HOLYSHOT_BENCH_CLOSE_MS=300

median() { sort -n | awk '{a[NR]=$1} END{print (NR? a[int((NR+1)/2)] : "n/a")}'; }
one() { # bin, key, label, args...
    local bin=$1 key=$2 label=$3; shift 3
    /usr/bin/time -o "$WORK/time" -f "%M %U %S" timeout 20 "$ICOUNT" "$bin" "$@" > "$WORK/log" 2>&1
    local t; t=$(grep -F "$label" "$WORK/log" | head -1 | sed 's/^\[ *//; s/ ms\].*//')
    read -r rss u s < "$WORK/time"
    local ic; ic=$(grep -F ICOUNT "$WORK/log" | awk '{printf "%.1f", $2/1e6}')
    echo "${t:-NaN} $rss $(awk -v u="$u" -v s="$s" 'BEGIN{print (u+s)*1000}') ${ic:-NaN}" >> "$WORK/r.$key"
    sleep 0.15
}
n=0; for b in $BINS; do n=$((n+1)); "$b" __dump "$WORK/warm.png" >/dev/null 2>&1; done
for i in $(seq 1 $RUNS); do
  n=0; for b in $BINS; do n=$((n+1))
    one "$b" "$n.screen" "thumbnail: first paint" screen
    one "$b" "$n.area"   "overlay: first paint"   area
    one "$b" "$n.editor" "editor: first paint"    __selftest "$SAMPLE" "$WORK/out.png"
  done
done
n=0; for b in $BINS; do n=$((n+1))
  echo "[$n] $b  ($(stat -c %s "$b") bytes)"
  for k in screen area editor; do
    f="$WORK/r.$n.$k"
    printf "    %-7s first frame %6s ms   peak RSS %6s KB   CPU %5s ms   %7s M instr\n" "$k" \
      "$(awk '{print $1}' "$f" | median)" "$(awk '{print $2}' "$f" | median)" "$(awk '{print $3}' "$f" | median)" "$(awk '{print $4}' "$f" | median)"
  done
done
kill $KW 2>/dev/null; wait $KW 2>/dev/null
touch "$WORK/done"
