#!/usr/bin/env bash
# Builds and installs HolyShot for the current user on KDE Plasma (Wayland).
#   binary     -> ~/.local/bin/holyshot
#   shortcuts  -> Print / Meta+Shift+4 = area,  Shift+Print / Meta+Shift+3 = screen under mouse
#                 (Print and Shift+Print are taken off Spectacle)
# Nothing runs in the background: each shortcut press starts the binary and it
# exits once the preview / editor is gone.
set -euo pipefail

SRC="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/holyshot"
APPS="$HOME/.local/share/applications"
ID=dev.holyshot

echo "==> Checking dependencies"
missing=()
/usr/bin/pkg-config --exists gtk4 2>/dev/null || missing+=(gtk4-devel)
[ -e /usr/lib64/libgtk4-layer-shell.so.0 ] || missing+=(gtk4-layer-shell)
[ -e /usr/lib64/libdeflate.so.0 ] || missing+=(libdeflate)
command -v wl-copy >/dev/null || missing+=(wl-clipboard)
if [ ${#missing[@]} -gt 0 ]; then
    echo "Missing packages: sudo dnf install ${missing[*]}"
    exit 1
fi
command -v cargo >/dev/null || { echo "cargo not found (install Rust: https://rustup.rs)"; exit 1; }

# gtk4-layer-shell-devel isn't required: link against the runtime library
# through a tiny local pkg-config shim instead.
mkdir -p "$SRC/build/pkgconfig" "$SRC/build/lib"
ln -sf /usr/lib64/libgtk4-layer-shell.so.0 "$SRC/build/lib/libgtk4-layer-shell.so"
cat > "$SRC/build/pkgconfig/gtk4-layer-shell-0.pc" <<EOF
libdir=$SRC/build/lib
Name: gtk4-layer-shell
Description: gtk4-layer-shell (shim)
Version: 1.3.0
Libs: -L\${libdir} -lgtk4-layer-shell
Cflags:
EOF

echo "==> Building"
(cd "$SRC" && PKG_CONFIG=/usr/bin/pkg-config \
    PKG_CONFIG_PATH="$SRC/build/pkgconfig:/usr/lib64/pkgconfig:/usr/share/pkgconfig" \
    cargo build --release --quiet)

echo "==> Removing the old Python version (if present)"
if [ -f "$BIN" ] && head -c 2 "$BIN" | grep -q '#!'; then
    "$BIN" quit 2>/dev/null || true
fi
rm -rf "$HOME/.local/share/holyshot"
rm -f "$APPS/holyshot.desktop" "$APPS/holyshot-grab.desktop" "$APPS/holyshot-editor.desktop" \
      "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/holyshot.desktop"
KGA=(gdbus call --session -d org.kde.kglobalaccel -o /kglobalaccel -m)
for a in "'_launch','HolyShot','Capture Area'" "'screen','HolyShot','Capture Whole Screen'"; do
    "${KGA[@]}" org.kde.KGlobalAccel.setShortcut "['holyshot.desktop',$a]" "@ai []" 4 >/dev/null 2>&1 || true
done
"${KGA[@]}" org.kde.KGlobalAccel.unregister holyshot.desktop _launch >/dev/null 2>&1 || true
"${KGA[@]}" org.kde.KGlobalAccel.unregister holyshot.desktop screen >/dev/null 2>&1 || true

echo "==> Installing $BIN"
mkdir -p "$BIN_DIR" "$APPS"
install -m 755 "$SRC/target/release/holyshot" "$BIN.new"
mv -f "$BIN.new" "$BIN"   # atomic, safe even if a copy is running

# KWin only hands screenshots to an executable named (by its real path) in a
# desktop file carrying X-KDE-DBUS-Restricted-Interfaces -- and it only looks
# at the *first* matching desktop file, in whatever order KDE's service cache
# happens to list them. So every desktop file that launches this binary must
# carry the key.
PERM="X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2"
rm -f "$APPS/$ID.capture.desktop"   # older installs used a separate file

cat > "$APPS/$ID.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=HolyShot
GenericName=Screenshot
Comment=Capture an area or the screen and annotate it
Exec=$BIN area
Icon=accessories-screenshot
Categories=Utility;Graphics;
Keywords=screenshot;capture;annotate;
StartupNotify=false
X-KDE-StartupNotify=false
$PERM
X-KDE-Shortcuts=Print,Meta+Shift+4
Actions=screen;

[Desktop Action screen]
Name=Capture Whole Screen
Exec=$BIN screen
X-KDE-Shortcuts=Shift+Print,Meta+Shift+3
EOF

cat > "$APPS/$ID.editor.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=HolyShot Editor
Comment=Annotate an image with arrows, boxes and text
Exec=$BIN edit %f
Icon=accessories-screenshot
MimeType=image/png;image/jpeg;image/webp;
NoDisplay=true
StartupNotify=false
$PERM
EOF

# Fail loudly if some other desktop file launches this binary without the key.
for f in "$APPS"/*.desktop; do
    if grep -q "^Exec=$BIN\b" "$f" && ! grep -q "^$PERM" "$f"; then
        echo "warning: $f launches $BIN without $PERM; KWin may refuse screenshots"
    fi
done

echo "==> Setting up shortcuts"
PRINT=$((0x01000009)); SHIFT=$((0x02000000)); META=$((0x10000000))
"${KGA[@]}" org.kde.KGlobalAccel.setShortcut \
    "['org.kde.spectacle.desktop','_launch','Spectacle','Launch Spectacle']" "[$((META | SHIFT | 0x53))]" 4 >/dev/null || true
"${KGA[@]}" org.kde.KGlobalAccel.setShortcut \
    "['org.kde.spectacle.desktop','FullScreenScreenShot','Spectacle','Capture Entire Desktop']" "@ai []" 4 >/dev/null || true
kbuildsycoca6 >/dev/null 2>&1 || true
sleep 1
reg() { # action, friendly name, keys
    "${KGA[@]}" org.kde.KGlobalAccel.doRegister "['$ID.desktop','$1','HolyShot','$2']" >/dev/null || true
    "${KGA[@]}" org.kde.KGlobalAccel.setShortcut "['$ID.desktop','$1','HolyShot','$2']" "$3" 4 >/dev/null || true
}
reg _launch "Capture Area" "[$PRINT, $((META | SHIFT | 0x34))]"
reg screen "Capture Whole Screen" "[$((SHIFT | PRINT)), $((META | SHIFT | 0x33))]"

echo
echo "Done."
echo "  Print  or  Meta+Shift+4          pick an area (drag, adjust, then Enter / Capture)"
echo "  Shift+Print  or  Meta+Shift+3    capture the screen the mouse is on"
echo "Screenshots go to ~/Pictures/Screenshots and the clipboard."
echo "Settings: ~/.config/holyshot/config.ini   Shortcuts: System Settings > Shortcuts > HolyShot"
