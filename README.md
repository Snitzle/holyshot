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

There's no background daemon: each key press starts the 0.8 MB binary, and it
exits once the preview / editor is gone, so it uses **no memory between
screenshots**. A second press while it's up is forwarded to the running copy
(GApplication).

### Benchmarks

`bench/run.sh RUNS BINARY...` runs each flow inside a hidden virtual KWin
(nothing appears on screen), interleaving binaries so background load hits
them equally. It reports time to first frame, peak RSS, CPU time and
**retired instructions** (via `perf_event_open`), which unlike wall time
doesn't move with machine load. 3440×1440 @ 115 %:

| per screenshot | first Rust release | now |
|---|---|---|
| Whole screen: instructions | 12 426 M | **562 M** (−95 %) |
| Whole screen: CPU time | 1390 ms | **130 ms** |
| Whole screen: peak memory | 78 MB | **60 MB** |
| While the preview shows | 61 MB (27 MB private) | **42 MB (8 MB private)** |
| Area: instructions | 774 M | **432 M** (−44 %) |
| Editor self-test: instructions | 4 501 M | **2 227 M** (−51 %) |
| Binary | 891 KB | **817 KB**, 8 fewer crates |
| Between screenshots | 0 | 0 |

The floor is GTK itself: linking its ~115 libraries costs ~96 M instructions
before `main`, and `gtk_init` another ~130 M.

### What got it there (each measured; see comments in the code)

- **Capture straight from KWin** over D-Bus (`org.kde.KWin.ScreenShot2`) on a
  worker thread started before GTK even initialises: ~45 ms, vs ~700 ms by
  shelling out to Spectacle. That thread also builds the preview image.
- **Cairo renderer** instead of GTK's default Vulkan: Vulkan costs ~320 ms and
  260 MB just to start, GL ~150 ms / 200 MB, Cairo ~1 ms / 83 MB, with
  pixel-identical output for what we draw.
- **Never make the CPU renderer resample big images per frame.** The preview
  and the editor view are pre-scaled once to their exact device-pixel size
  (that alone was 60 ms of CPU *per animation frame*). The scaler halves with
  packed-pixel (SWAR) 2×2 averages, then does one 3-tap area-average pass with
  weights summing to 256 (a shift, not a division), built with
  `target-cpu=native`.
- **The preview slides by moving its layer surface** (margin), so the
  animation repaints nothing; its shadow + border are rendered once.
- **PNG via libdeflate level 1**, hand-written PNG container:
  same file size as zlib level 6 for 18 % of the instructions
  (`holyshot __bench-png`), and encoded **in place** in the capture's own
  buffer, so saving needs no second full-size buffer.
- **The preview doesn't keep the full image**: the save thread owns it; the
  editor and drag-and-drop then use the saved file.
- **No portal settings lookup at start-up** (`GDK_DEBUG=no-portals`, −45 ms);
  the KDE file dialog is still used, via the FileChooser portal directly.
- **Icon theme deferred**: loading Breeze icons costs ~45 ms inside
  `gtk_init`; GTK is pointed at a mirrored config dir naming the tiny hicolor
  theme, and the editor (the only window with icons) switches to the real one.
- **fontconfig warmed on a side thread** while GTK starts (−19 ms on the first
  text the overlay draws).

Tried and rejected (measured worse): BGRx textures (force a per-pixel
conversion), a zero-copy Cairo surface for the overlay (GTK caches a copy
anyway), `opt-level = "s"` (−11 % size, 2× slower pixel loops).

Set `HOLYSHOT_TRACE=1` to print timings with per-thread instruction counts.
Dev-only commands: `__dump FILE` (capture without UI), `__bench-png FILE…`,
`__bench-scale FILE`, `__selftest IN OUT` and `__selftest-area` (drive the
editor / picker with synthetic input).
