# HolyShot

Mac-style screenshots for KDE Plasma (Wayland), in Rust + GTK4.

> **This project is vibe coded.** I don't like Spectacle, KDE's built-in
> screenshot tool, so I described what I wanted (macOS-style capture with a
> simple annotation editor) to an AI coding assistant ([Claude Code](https://claude.com/claude-code))
> and it wrote this. I haven't reviewed the code line by line. It works on my
> machine (Fedora 44, Plasma 6.7); use it at your own risk.

| Shortcut | What it does |
|---|---|
| **Print** / **Meta+Shift+4** | Dim the screen and drag out a box. Move/resize it at leisure, then **Enter**, double-click or **Capture**. Esc / right-click cancels. |
| **Shift+Print** / **Meta+Shift+3** | Capture the whole screen the mouse is on. |

A preview slides into the bottom-right corner: **click** it to annotate,
**drag** it into another app to drop the file, hover to keep it, right-click
for Copy / Show in Folder / Move to Trash. Every shot is saved to
`~/Pictures/Screenshots` and copied to the clipboard.

The editor has Arrow, Box, Circle, Draw, Highlight, Text, Steps (1, 2, 3…) and
Pixelate, 8 colours and 3 sizes. Keys: `V A R E P H T N B` pick tools, `1`–`8`
colours, `[` `]` size, Shift constrains, arrows nudge, Delete removes, Ctrl+Z /
Ctrl+Shift+Z, Ctrl+C copies, Ctrl+S saves, **Save** saves and closes.

## Install

```sh
./install.sh      # builds, installs ~/.local/bin/holyshot, binds the shortcuts
./uninstall.sh    # removes it and gives Print back to Spectacle
```

Needs `gtk4-devel`, `gtk4-layer-shell` (runtime lib only), `wl-clipboard` and
Rust. Settings: `~/.config/holyshot/config.ini`.

## How it stays fast and light

There's no background daemon: each key press starts the 0.9 MB binary, and it
exits once the preview / editor is gone, so it uses **no memory between
screenshots**. A second press while it's up is forwarded to the running copy
(GApplication).

Measured on this machine (3440×1440 @ 115 %), key-press launch → first frame:

| | first Rust build (GTK defaults) | now |
|---|---|---|
| Whole screen → preview visible | ~1100 ms | **~64 ms** |
| Area → dimmed overlay visible | ~380 ms | **~105 ms** |
| Peak memory, area picker | ~260 MB | **~105 MB** |
| Memory while preview shows | | **~65 MB** (~30 MB of it private) |
| Memory between screenshots | 113 MB (old Python daemon) | **0** |

What got it there (each measured, see comments in the code):

- **Capture straight from KWin** over D-Bus (`org.kde.KWin.ScreenShot2`) on a
  worker thread started before GTK even initialises: ~45 ms, vs ~700 ms by
  shelling out to Spectacle.
- **Cairo renderer** instead of GTK's default Vulkan: Vulkan costs ~320 ms and
  260 MB just to start, GL ~150 ms / 200 MB, Cairo ~1 ms / 83 MB, with
  pixel-identical output for what we draw.
- **No portal settings lookup at start-up** (`GDK_DEBUG=no-portals`, −45 ms);
  the KDE file dialog is still used, via the FileChooser portal directly.
- **Icon theme deferred**: loading Breeze icons costs ~45 ms inside
  `gtk_init`; GTK is pointed at a mirrored config dir naming the tiny hicolor
  theme, and the editor (the only window with icons) switches to the real one.
- **fontconfig warmed on a side thread** while GTK starts (−19 ms on the first
  text the overlay draws).
- **Zero-copy pixels**: the capture buffer is wrapped as textures and sliced
  per monitor without copying; PNGs are streamed row by row.
- **PNG settings benchmarked** (`holyshot __bench-png`): Balanced + no filter
  gives files ~30 % smaller than the fast settings for ~100 ms on a background
  thread; clipboard copies from the editor use the fast settings.

Set `HOLYSHOT_TRACE=1` to print timings. Dev-only commands: `__dump FILE`
(capture without UI), `__bench-png FILE…`, `__selftest IN OUT` and
`__selftest-area` (drive the editor / picker with synthetic input).
