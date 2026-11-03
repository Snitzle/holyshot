#!/usr/bin/env bash
# Removes HolyShot and gives Print / Shift+Print back to Spectacle.
set -uo pipefail
ID=dev.holyshot
KGA=(gdbus call --session -d org.kde.kglobalaccel -o /kglobalaccel -m)
for a in "'_launch','HolyShot','Capture Area'" "'screen','HolyShot','Capture Whole Screen'" \
         "'window','HolyShot','Capture Window'" "'record','HolyShot','Record Screen'"; do
    "${KGA[@]}" org.kde.KGlobalAccel.setShortcut "['$ID.desktop',$a]" "@ai []" 4 >/dev/null
done
for a in _launch screen window record; do
    "${KGA[@]}" org.kde.KGlobalAccel.unregister "$ID.desktop" $a >/dev/null
done
"${KGA[@]}" org.kde.KGlobalAccel.setShortcut \
    "['org.kde.spectacle.desktop','_launch','Spectacle','Launch Spectacle']" "[$((0x01000009)), $((0x12000053))]" 4 >/dev/null
"${KGA[@]}" org.kde.KGlobalAccel.setShortcut \
    "['org.kde.spectacle.desktop','FullScreenScreenShot','Spectacle','Capture Entire Desktop']" "[$((0x03000009))]" 4 >/dev/null
rm -f "$HOME/.local/bin/holyshot" \
      "$HOME/.local/share/applications/$ID.desktop" \
      "$HOME/.local/share/applications/$ID.editor.desktop" \
      "$HOME/.local/share/applications/$ID.capture.desktop"
kbuildsycoca6 >/dev/null 2>&1
echo "HolyShot removed (your screenshots and ~/.config/holyshot were left alone)."
