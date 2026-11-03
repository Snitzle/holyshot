use gtk::{gdk, gio, glib, graphene, gsk, pango, prelude::*};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

// ---- tracing (HOLYSHOT_TRACE=1 prints timings to stderr) --------------------
static START: OnceLock<(Instant, bool)> = OnceLock::new();

pub fn trace_init() {
    START.get_or_init(|| (Instant::now(), std::env::var_os("HOLYSHOT_TRACE").is_some()));
}

pub fn tracing() -> bool {
    START.get().is_some_and(|s| s.1)
}

pub fn trace(what: &str) {
    if let Some((t, true)) = START.get() {
        use std::io::Write;
        let (total, delta) = icount::sample();
        let _ = writeln!(
            std::io::stderr(),
            "[{:7.1} ms] [{:>14}] {what}",
            t.elapsed().as_secs_f64() * 1000.0,
            format!("{:.1}M +{:.1}M", total as f64 / 1e6, delta as f64 / 1e6)
        );
    }
}

/// Instructions retired by this thread so far (0 if unavailable).
pub fn instructions() -> u64 {
    icount::read_now()
}

/// Per-thread retired-instruction counter (perf_event_open), for tracing only.
mod icount {
    use std::cell::Cell;
    use std::ffi::{c_int, c_long};
    unsafe extern "C" {
        fn syscall(num: c_long, ...) -> c_long;
        fn read(fd: c_int, buf: *mut u8, n: usize) -> isize;
    }
    thread_local! {
        static FD: Cell<c_int> = const { Cell::new(-2) };
        static LAST: Cell<u64> = const { Cell::new(0) };
    }
    fn open() -> c_int {
        let mut attr = [0u8; 128];
        attr[4..8].copy_from_slice(&128u32.to_ne_bytes()); // size
        attr[8..16].copy_from_slice(&1u64.to_ne_bytes()); // PERF_COUNT_HW_INSTRUCTIONS (type 0 = HARDWARE)
        attr[40..48].copy_from_slice(&((1u64 << 5) | (1u64 << 6)).to_ne_bytes()); // exclude kernel/hv
        // SAFETY: perf_event_open(attr, pid=0 (this thread), cpu=-1, group=-1, flags=0)
        unsafe { syscall(298, attr.as_ptr(), 0 as c_int, -1 as c_int, -1 as c_int, 0 as c_long) as c_int }
    }
    pub fn read_now() -> u64 {
        let fd = FD.with(|f| {
            if f.get() == -2 {
                f.set(open());
            }
            f.get()
        });
        if fd < 0 {
            return 0;
        }
        let mut buf = [0u8; 8];
        // SAFETY: reading 8 bytes from our own perf fd into a local buffer
        unsafe { read(fd, buf.as_mut_ptr(), 8) };
        u64::from_ne_bytes(buf)
    }

    pub fn sample() -> (u64, u64) {
        let fd = FD.with(|f| {
            if f.get() == -2 {
                f.set(open());
            }
            f.get()
        });
        if fd < 0 {
            return (0, 0);
        }
        let mut buf = [0u8; 8];
        // SAFETY: reading 8 bytes from our own perf fd into a local buffer
        unsafe { read(fd, buf.as_mut_ptr(), 8) };
        let n = u64::from_ne_bytes(buf);
        let d = n - LAST.with(|l| l.replace(n));
        (n, d)
    }
}

pub fn trace_once(what: &'static str) {
    static SEEN: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    if let Some((_, true)) = START.get() {
        let mut seen = SEEN.lock().unwrap();
        if !seen.contains(&what) {
            seen.push(what);
            trace(what);
        }
    }
}

/// Count events when tracing; totals are printed at exit (`trace_counts`).
pub fn count(what: &'static str) {
    static_counts(|m| *m.entry(what).or_insert(0) += 1);
}

fn static_counts(f: impl FnOnce(&mut std::collections::BTreeMap<&'static str, u64>)) {
    static C: std::sync::Mutex<std::collections::BTreeMap<&'static str, u64>> = std::sync::Mutex::new(std::collections::BTreeMap::new());
    if let Some((_, true)) = START.get() {
        f(&mut C.lock().unwrap());
    }
}

pub fn trace_counts() {
    static_counts(|m| {
        for (k, v) in m.iter() {
            trace(&format!("count {k} = {v}"));
        }
    });
}

/// Trace when a window's first frame has been painted.
pub fn trace_first_paint(win: &impl IsA<gtk::Widget>, label: &'static str) {
    if !matches!(START.get(), Some((_, true))) {
        return;
    }
    win.connect_realize(move |w| {
        if let Some(fc) = w.frame_clock() {
            fc.connect_after_paint(move |_| trace_once(label));
        }
    });
}

/// Benchmark hook: with HOLYSHOT_TRACE set, HOLYSHOT_BENCH_CLOSE_MS=N makes
/// the picker cancel itself after N ms (so timing runs don't grab input).
pub fn bench_close_ms() -> Option<u64> {
    if !matches!(START.get(), Some((_, true))) {
        return None;
    }
    std::env::var("HOLYSHOT_BENCH_CLOSE_MS").ok()?.parse().ok()
}

// ---- GTK start-up tuning ------------------------------------------------------
static ORIG_ENV: OnceLock<Vec<(&'static str, Option<std::ffi::OsString>)>> = OnceLock::new();

/// Measured on Plasma 6.7 / GTK 4.22, launch to first painted overlay:
/// Vulkan 377 ms / 260 MB, GL 280 ms / 203 MB, Cairo 100 ms / 83 MB, with
/// identical output (we only composite a texture and a few shapes). Skipping
/// GTK's portal settings lookup saves another ~45 ms; we talk to the file
/// chooser portal ourselves instead (see `portal_save_file`).
pub fn tune_gtk_env() {
    let keys = ["GSK_RENDERER", "GDK_DEBUG", "GTK_THEME", "XDG_CONFIG_HOME"];
    ORIG_ENV.get_or_init(|| keys.iter().map(|k| (*k, std::env::var_os(k))).collect());
    // NB: no glib path helpers before this point: glib caches them on first use.
    let cfg = real_config_dir();
    let ini_text = std::fs::read_to_string(cfg.join("gtk-4.0/settings.ini")).unwrap_or_default();
    let ini = glib::KeyFile::new();
    let _ = ini.load_from_data(&ini_text, glib::KeyFileFlags::NONE);
    // SAFETY: called first thing in main, before any other thread exists.
    unsafe {
        if std::env::var_os("GSK_RENDERER").is_none() {
            std::env::set_var("GSK_RENDERER", "cairo");
        }
        // Without the portal GTK misses the desktop's dark-mode preference,
        // so take it from settings.ini (which Plasma keeps in sync).
        if std::env::var_os("GTK_THEME").is_none()
            && ini.boolean("Settings", "gtk-application-prefer-dark-theme").unwrap_or(false)
        {
            let theme = ini.string("Settings", "gtk-theme-name").map(|s| s.to_string()).unwrap_or_else(|_| "Adwaita".into());
            std::env::set_var("GTK_THEME", format!("{theme}:dark"));
        }
        let dbg = std::env::var("GDK_DEBUG").unwrap_or_default();
        if !dbg.contains("portals") {
            std::env::set_var("GDK_DEBUG", if dbg.is_empty() { "no-portals".to_string() } else { format!("{dbg},no-portals") });
        }
        if let Some(real) = ini.string("Settings", "gtk-icon-theme-name").ok().filter(|t| t != "hicolor") {
            if let Some(dir) = light_gtk_config(&cfg, &ini_text) {
                std::env::set_var("XDG_CONFIG_HOME", &dir);
                let _ = REAL_ICON_THEME.set(real.to_string());
            }
        }
    }
}

static REAL_CONFIG: OnceLock<PathBuf> = OnceLock::new();
static REAL_ICON_THEME: OnceLock<String> = OnceLock::new();

/// The user's config dir as it was at launch (see `light_gtk_config`).
pub fn real_config_dir() -> PathBuf {
    REAL_CONFIG
        .get_or_init(|| {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"))
        })
        .clone()
}

/// GTK loads the configured icon theme during gtk_init, and Breeze costs
/// ~45 ms there (measured), yet only the editor shows themed icons. So GTK is
/// pointed at a private config dir in $XDG_RUNTIME_DIR that mirrors the real
/// one (symlinks) except that settings.ini names the tiny hicolor theme; the
/// editor switches to the real theme when it opens (`use_real_icon_theme`).
/// Our own config and child processes keep using the real dir.
fn light_gtk_config(cfg: &Path, ini_text: &str) -> Option<PathBuf> {
    use std::os::unix::fs::symlink;
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    let dir = runtime.join("holyshot").join("config");
    let light: String = ini_text
        .lines()
        .map(|l| if l.trim_start().starts_with("gtk-icon-theme-name") { "gtk-icon-theme-name=hicolor" } else { l })
        .collect::<Vec<_>>()
        .join("\n");
    let ini_path = dir.join("gtk-4.0/settings.ini");
    // Rebuild when the source dir, its entries (dir mtime) or settings change.
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let stamp = format!("{}\n{:?}\n{:?}", cfg.display(), mtime(cfg), mtime(&cfg.join("gtk-4.0")));
    let stamp_path = dir.join(".holyshot-mirror");
    if std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str())
        && std::fs::read_to_string(&ini_path).ok().as_deref() == Some(light.as_str())
    {
        return Some(dir); // already up to date
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("gtk-4.0")).ok()?;
    // mirror every top-level entry (fontconfig, mimeapps.list, user-dirs.dirs, ...)
    for e in std::fs::read_dir(cfg).ok()?.flatten() {
        if e.file_name() != "gtk-4.0" {
            let _ = symlink(e.path(), dir.join(e.file_name()));
        }
    }
    // and gtk-4.0 itself (gtk.css, colors.css, bookmarks, ...) minus settings.ini
    for e in std::fs::read_dir(cfg.join("gtk-4.0")).ok()?.flatten() {
        if e.file_name() != "settings.ini" {
            let _ = symlink(e.path(), dir.join("gtk-4.0").join(e.file_name()));
        }
    }
    std::fs::write(&ini_path, light).ok()?;
    std::fs::write(&stamp_path, stamp).ok()?;
    Some(dir)
}

/// Called when the editor opens: load the real icon theme now.
pub fn use_real_icon_theme() {
    if let (Some(s), Some(name)) = (gtk::Settings::default(), REAL_ICON_THEME.get()) {
        if s.gtk_icon_theme_name().as_deref() != Some(name.as_str()) {
            s.set_gtk_icon_theme_name(Some(name));
        }
    }
}

#[link(name = "fontconfig")]
unsafe extern "C" {
    fn FcInit() -> std::ffi::c_int;
}

pub fn warm_fontconfig() {
    // SAFETY: FcInit takes no arguments and is safe to call from any thread.
    unsafe {
        FcInit();
    }
}

/// A launch context that gives child apps the environment we started with.
pub fn launch_context() -> gio::AppLaunchContext {
    let ctx = gio::AppLaunchContext::new();
    for (k, v) in ORIG_ENV.get().into_iter().flatten() {
        match v {
            Some(v) => ctx.setenv(k, v),
            None => ctx.unsetenv(k),
        }
    }
    ctx
}

/// A child process with the environment we started with.
pub fn command(name: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(name);
    restore_env(&mut cmd);
    cmd
}

fn restore_env(cmd: &mut std::process::Command) -> &mut std::process::Command {
    for (k, v) in ORIG_ENV.get().into_iter().flatten() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd
}

/// Ask the desktop's file chooser (KDE's own dialog on Plasma) for a save path.
/// Calls `done(None)` if cancelled; `fallback()` if the portal isn't available.
pub fn portal_save_file(
    title: &str,
    name: &str,
    folder: &Path,
    done: impl FnOnce(Option<PathBuf>) + 'static,
    fallback: impl FnOnce() + 'static,
) {
    use std::cell::RefCell;
    use std::rc::Rc;
    let Ok(conn) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else { return fallback() };
    let token = format!("holyshot{}", glib::random_int());
    let sender = conn.unique_name().map(|s| s.trim_start_matches(':').replace('.', "_")).unwrap_or_default();
    let req_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

    type Done = Box<dyn FnOnce(Option<PathBuf>)>;
    let done: Rc<RefCell<Option<Done>>> = Rc::new(RefCell::new(Some(Box::new(done))));
    let sub: Rc<RefCell<Option<gio::SignalSubscription>>> = Rc::default();
    let (d2, s2) = (done.clone(), sub.clone());
    // Subscribe before calling so the response can't be missed.
    let subscription = conn.subscribe_to_signal(
        Some("org.freedesktop.portal.Desktop"),
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        Some(&req_path),
        None,
        gio::DBusSignalFlags::NONE,
        move |sig| {
            let params = sig.parameters;
            let code: u32 = params.child_value(0).get().unwrap_or(2);
            let results = glib::VariantDict::new(Some(&params.child_value(1)));
            let path = (code == 0)
                .then(|| results.lookup::<Vec<String>>("uris").ok().flatten())
                .flatten()
                .and_then(|u| u.into_iter().next())
                .and_then(|u| gio::File::for_uri(&u).path());
            if let Some(f) = d2.borrow_mut().take() {
                f(path);
            }
            s2.borrow_mut().take();
        },
    );
    *sub.borrow_mut() = Some(subscription);

    let opts = glib::VariantDict::new(None);
    opts.insert("handle_token", &token);
    opts.insert("modal", true);
    opts.insert("current_name", name);
    let mut dir = folder.as_os_str().as_encoded_bytes().to_vec();
    dir.push(0);
    opts.insert_value("current_folder", &dir.to_variant());
    let filters: Vec<(String, Vec<(u32, String)>)> = vec![
        ("PNG image".into(), vec![(0, "*.png".into())]),
        ("JPEG image".into(), vec![(0, "*.jpg".into()), (0, "*.jpeg".into())]),
    ];
    opts.insert_value("filters", &filters.to_variant());
    let params = glib::Variant::tuple_from_iter(["".to_variant(), title.to_variant(), opts.end()]);
    let fallback = RefCell::new(Some(fallback));
    conn.call(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.FileChooser",
        "SaveFile",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
        move |r| {
            if r.is_err() {
                sub.borrow_mut().take();
                done.borrow_mut().take();
                if let Some(f) = fallback.borrow_mut().take() {
                    f();
                }
            }
        },
    );
}

// ---- config: ~/.config/holyshot/config.ini ---------------------------------
pub struct Config {
    pub save_dir: PathBuf,
    pub copy_to_clipboard: bool,
    pub thumbnail_seconds: f64,
    pub record_dir: PathBuf,
    pub record_encoder: String,
    pub remember_area: bool,
}

const DEFAULT_CONFIG: &str = "\
[holyshot]
# Where screenshots are saved (~ is expanded)
save_dir=~/Pictures/Screenshots
# Also copy each new screenshot to the clipboard
copy_to_clipboard=true
# How long the bottom-right preview stays up (hovering pauses it)
thumbnail_seconds=6
# Where screen recordings are saved
record_dir=~/Videos/Screencasts
# auto, or one of: nvh264enc vah264lpenc vah264enc x264enc openh264enc
record_encoder=auto
# Start the area picker with the last box you used (Delete clears it)
remember_area=true
";

pub fn config() -> &'static Config {
    static CFG: OnceLock<Config> = OnceLock::new();
    CFG.get_or_init(|| {
        let path = real_config_dir().join("holyshot").join("config.ini");
        let kf = glib::KeyFile::new();
        if kf.load_from_file(&path, glib::KeyFileFlags::NONE).is_err() {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(&path, DEFAULT_CONFIG);
            let _ = kf.load_from_data(DEFAULT_CONFIG, glib::KeyFileFlags::NONE);
        }
        let g = "holyshot";
        let dir = kf.string(g, "save_dir").map(|s| s.to_string()).unwrap_or_else(|_| "~/Pictures/Screenshots".into());
        let s = |k: &str, d: &str| kf.string(g, k).map(|s| s.to_string()).unwrap_or_else(|_| d.into());
        Config {
            save_dir: expand(&dir),
            copy_to_clipboard: kf.boolean(g, "copy_to_clipboard").unwrap_or(true),
            thumbnail_seconds: kf.double(g, "thumbnail_seconds").unwrap_or(6.0).max(1.0),
            record_dir: expand(&s("record_dir", "~/Videos/Screencasts")),
            record_encoder: s("record_encoder", "auto"),
            remember_area: kf.boolean(g, "remember_area").unwrap_or(true),
        }
    })
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => glib::home_dir().join(rest),
        None if p == "~" => glib::home_dir(),
        None => PathBuf::from(p),
    }
}

fn last_area_file() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| glib::home_dir().join(".local/state"))
        .join("holyshot/last-area")
}

/// The last box picked, in global logical coordinates.
pub fn last_area() -> Option<(i32, i32, i32, i32)> {
    if !config().remember_area {
        return None;
    }
    let s = std::fs::read_to_string(last_area_file()).ok()?;
    let v: Vec<i32> = s.split_whitespace().filter_map(|t| t.parse().ok()).collect();
    match v[..] {
        [x, y, w, h] if w > 0 && h > 0 => Some((x, y, w, h)),
        _ => None,
    }
}

pub fn save_last_area((x, y, w, h): (i32, i32, i32, i32)) {
    if !config().remember_area {
        return;
    }
    let f = last_area_file();
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let _ = std::fs::write(f, format!("{x} {y} {w} {h}\n"));
}

/// e.g. `~/Pictures/Screenshots/Screenshot 2026-11-03 at 12.34.56.png`
pub fn unique_path(dir: &Path, prefix: &str, ext: &str) -> PathBuf {
    let _ = std::fs::create_dir_all(dir);
    let stamp = glib::DateTime::now_local()
        .and_then(|d| d.format(&format!("{prefix} %Y-%m-%d at %H.%M.%S")))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| prefix.into());
    let mut p = dir.join(format!("{stamp}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stamp} ({n}).{ext}"));
        n += 1;
    }
    p
}

// ---- desktop integration ----------------------------------------------------
pub fn notify(summary: &str, body: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr(), "holyshot: {summary} {body}"); // never panic on a closed stderr
    let _ = restore_env(&mut std::process::Command::new("notify-send"))
        .args(["-a", "HolyShot", "-i", "accessories-screenshot", summary, body])
        .spawn();
}

/// Wayland clipboards vanish with the process that owns them, and we exit as
/// soon as the preview goes away, so hand the PNG to wl-copy, which keeps
/// serving it in the background.
pub fn clipboard_png_file(path: &Path) -> bool {
    let Ok(f) = std::fs::File::open(path) else { return false };
    restore_env(&mut std::process::Command::new("wl-copy"))
        .args(["--type", "image/png"])
        .stdin(f)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

pub fn clipboard_png_bytes(png: Vec<u8>) -> bool {
    use std::io::Write;
    let child = restore_env(&mut std::process::Command::new("wl-copy"))
        .args(["--type", "image/png"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match child {
        Ok(mut c) => {
            let ok = c.stdin.take().map(|mut s| s.write_all(&png).is_ok()).unwrap_or(false);
            let _ = c.wait(); // wl-copy forks and the parent exits immediately
            ok
        }
        Err(_) => false,
    }
}

pub fn show_in_folder(path: &Path) {
    let uri = gio::File::for_path(path).uri();
    let ok = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .and_then(|c| {
            c.call_sync(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
                "ShowItems",
                Some(&(vec![uri.to_string()], "").to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                3000,
                gio::Cancellable::NONE,
            )
        })
        .is_ok();
    if !ok {
        if let Some(dir) = path.parent() {
            let _ = gio::AppInfo::launch_default_for_uri(&gio::File::for_path(dir).uri(), Some(&launch_context()));
        }
    }
}

/// `SourceId::remove` panics if the source already ran or was removed;
/// this doesn't.
pub fn remove_source(id: glib::SourceId) {
    if glib::MainContext::default().find_source_by_id(&id).is_some() {
        id.remove();
    }
}

pub fn monitors() -> Vec<gdk::Monitor> {
    let Some(display) = gdk::Display::default() else { return vec![] };
    let list = display.monitors();
    (0..list.n_items()).filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>()).collect()
}

// ---- drawing helpers ----------------------------------------------------------
pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> gdk::RGBA {
    gdk::RGBA::new(r, g, b, a)
}

pub fn hex(s: &str) -> gdk::RGBA {
    gdk::RGBA::parse(s).unwrap_or(gdk::RGBA::RED)
}

pub fn rect(x: f32, y: f32, w: f32, h: f32) -> graphene::Rect {
    graphene::Rect::new(x, y, w, h)
}

pub fn rounded(r: graphene::Rect, radius: f32) -> gsk::RoundedRect {
    gsk::RoundedRect::from_rect(r, radius)
}

/// Layer-shell namespace for all our surfaces. KWin turns a namespace it
/// doesn't recognise into a *normal* window, and its default open/close
/// animation (Scale) then grows every one of them from 80 % at its centre,
/// fading it in: the picker visibly zooms out from the middle of the screen,
/// fights the preview's own slide, and the picker's 200 ms close animation
/// could land in a recording's first frames. "dock" (what panels use) is
/// skipped by Scale, Fade and Fading Popups, and keeps keyboard focus.
pub const LAYER_NAMESPACE: &str = "dock";

pub fn layout(widget: &impl IsA<gtk::Widget>, text: &str, pt: f64, bold: bool) -> pango::Layout {
    let l = widget.create_pango_layout(Some(text));
    let mut fd = widget.pango_context().font_description().unwrap_or_default();
    fd.set_size((pt * pango::SCALE as f64) as i32);
    if bold {
        fd.set_weight(pango::Weight::Bold);
    }
    l.set_font_description(Some(&fd));
    l
}

/// Text in a dark rounded pill (labels on the overlay).
pub fn pill(sn: &gtk::Snapshot, layout: &pango::Layout, x: f32, y: f32, pad_x: f32, pad_y: f32, radius: f32) -> graphene::Rect {
    let (lw, lh) = layout.pixel_size();
    let r = rect(x, y, lw as f32 + 2.0 * pad_x, lh as f32 + 2.0 * pad_y);
    sn.push_rounded_clip(&rounded(r, radius));
    sn.append_color(&rgba(0.08, 0.08, 0.08, 0.86), &r);
    sn.pop();
    sn.save();
    sn.translate(&graphene::Point::new(x + pad_x, y + pad_y));
    sn.append_layout(layout, &gdk::RGBA::WHITE);
    sn.restore();
    r
}
