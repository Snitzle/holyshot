//! Screen capture straight from KWin (org.kde.KWin.ScreenShot2 over D-Bus).
//!
//! KWin only answers executables named in a desktop file carrying
//! `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2`; install.sh
//! writes that for this binary.

use gtk::{gio, glib, prelude::*};
use std::io::Read;

use crate::img::Img;

pub enum Target {
    /// Every screen in one image (at the highest screen scale).
    Workspace,
    /// The screen KWin considers active (follows the mouse by default).
    Active,
    /// One window by KWin's internal id, with its frame and shadow, even if
    /// other windows cover it.
    Window(String),
    /// A box in global logical coordinates, live (the self-timer's capture).
    Area(i32, i32, u32, u32),
}

/// A window the picker can highlight (logical, global coordinates).
#[derive(Clone, Debug)]
pub struct Win {
    pub id: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub struct Capture {
    pub img: Img,
    /// Output name (e.g. "HDMI-A-1"), only for `Target::Active`.
    pub screen: Option<String>,
    /// Preview-sized copy, when computed ahead of time.
    pub preview: Option<Img>,
    /// Visible windows, topmost first (area picks only).
    pub windows: Vec<Win>,
}

const NAME: &str = "org.kde.KWin";
const PATH: &str = "/org/kde/KWin/ScreenShot2";
const IFACE: &str = "org.kde.KWin.ScreenShot2";

/// True if another process already owns our application id, i.e. this
/// process is about to forward its command line and exit. Lets the
/// speculative capture in `main` skip work that would be thrown away.
pub fn other_instance_running(conn: &gio::DBusConnection) -> bool {
    let reply = conn.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetNameOwner",
        Some(&(crate::APP_ID,).to_variant()),
        Some(glib::VariantTy::new("(s)").unwrap()),
        gio::DBusCallFlags::NONE,
        500,
        gio::Cancellable::NONE,
    );
    match reply {
        Ok(v) => {
            let owner: String = v.child_value(0).get().unwrap_or_default();
            // GApplication registration may be racing us on the main thread;
            // if *we* own the name we are the primary instance.
            Some(owner.as_str()) != conn.unique_name().as_deref()
        }
        Err(_) => false,
    }
}

pub fn session() -> Result<gio::DBusConnection, String> {
    gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(|e| e.to_string())
}

pub fn grab(conn: &gio::DBusConnection, target: &Target) -> Result<Capture, String> {
    crate::util::trace("capture: start");
    let (mut reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;

    let opts = glib::VariantDict::new(None);
    opts.insert("native-resolution", true);
    opts.insert("include-cursor", false);
    let window = matches!(target, Target::Window(_));
    opts.insert("include-decoration", window);
    opts.insert("include-shadow", window);

    let fds = gio::UnixFDList::new();
    let handle = fds.append(&writer).map_err(|e| e.to_string())?;
    drop(writer);

    let fd = glib::variant::Handle(handle).to_variant();
    let (method, params) = match target {
        Target::Workspace => ("CaptureWorkspace", glib::Variant::tuple_from_iter([opts.end(), fd])),
        Target::Active => ("CaptureActiveScreen", glib::Variant::tuple_from_iter([opts.end(), fd])),
        Target::Window(id) => ("CaptureWindow", glib::Variant::tuple_from_iter([id.to_variant(), opts.end(), fd])),
        &Target::Area(x, y, w, h) => (
            "CaptureArea",
            glib::Variant::tuple_from_iter([x.to_variant(), y.to_variant(), w.to_variant(), h.to_variant(), opts.end(), fd]),
        ),
    };

    // KWin writes the pixels *before* replying, so the pipe must be drained
    // concurrently or a big image fills the pipe buffer and both sides hang.
    let drain = std::thread::spawn(move || {
        // Typical capture is ~20 MB; reserving avoids a dozen regrowth copies.
        // Untouched capacity is never faulted in, so over-reserving is free.
        let mut buf = Vec::with_capacity(32 << 20);
        reader.read_to_end(&mut buf).map(|_| buf)
    });

    let reply = conn.call_with_unix_fd_list_sync(
        Some(NAME),
        PATH,
        IFACE,
        method,
        Some(&params),
        Some(glib::VariantTy::new("(a{sv})").unwrap()),
        gio::DBusCallFlags::NONE,
        5000,
        Some(&fds),
        gio::Cancellable::NONE,
    );
    // The fd list holds a dup of the write end; it must go before we can see EOF.
    drop(fds);
    let data = drain.join().map_err(|_| "capture reader panicked".to_string())?;
    let (reply, _) = reply.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("not authorized") {
            "KWin refused the screenshot (not authorized). Re-run install.sh so the desktop file matches this binary.".into()
        } else {
            msg
        }
    })?;
    let mut data = data.map_err(|e| e.to_string())?;

    let dict = glib::VariantDict::new(Some(&reply.child_value(0)));
    let get_u = |k: &str| dict.lookup::<u32>(k).ok().flatten().unwrap_or(0) as usize;
    let (w, h, stride) = (get_u("width"), get_u("height"), get_u("stride"));
    let scale = dict.lookup::<f64>("scale").ok().flatten().unwrap_or(1.0);
    let screen = dict.lookup::<String>("screen").ok().flatten();
    if w == 0 || h == 0 || data.len() < stride * h {
        return Err(format!("bad capture ({w}x{h}, {} bytes)", data.len()));
    }
    data.truncate(stride * h);
    data.shrink_to_fit(); // mremap for large allocations: no copy
    crate::util::trace("capture: done");
    let mut img = Img::from_vec(data, w, h, stride, scale);
    img.opaque = !window; // the shadow is translucent
    Ok(Capture { img, screen, preview: None, windows: vec![] })
}

/// The windows on the current desktop, topmost first, so the picker can
/// highlight the one under the mouse. KWin has no D-Bus call for this, so
/// we run a two-line KWin script that calls back to us with the list.
pub fn windows(conn: &gio::DBusConnection) -> Result<Vec<Win>, String> {
    let me = conn.unique_name().ok_or("no bus name")?;
    let dir = glib::user_runtime_dir().join("holyshot");
    let _ = std::fs::create_dir_all(&dir);
    let pid = std::process::id();
    let file = dir.join(format!("windows-{pid}.js"));
    let plugin = format!("holyshot-windows-{pid}");
    std::fs::write(&file, WINDOWS_JS.replace("@ME@", &me)).map_err(|e| e.to_string())?;

    // Catch the callback in GDBus's worker thread: this thread has no main
    // loop, and the main thread may still be starting GTK.
    let (tx, rx) = std::sync::mpsc::channel::<gio::DBusMessage>();
    let tx = std::sync::Mutex::new(tx);
    let filter = conn.add_filter(move |_, msg, incoming| {
        if incoming && msg.message_type() == gio::DBusMessageType::MethodCall && msg.path().as_deref() == Some(WINDOWS_PATH) {
            let _ = tx.lock().map(|t| t.send(msg.clone()));
            return None;
        }
        Some(msg.clone())
    });
    let call = |path: &str, iface: &str, method: &str, args: Option<glib::Variant>| {
        conn.call_sync(Some("org.kde.KWin"), path, iface, method, args.as_ref(), None, gio::DBusCallFlags::NONE, 2000, gio::Cancellable::NONE)
    };
    let scripting = |method: &str, args: glib::Variant| call("/Scripting", "org.kde.kwin.Scripting", method, Some(args));
    let _ = scripting("unloadScript", (&plugin,).to_variant()); // stale from a crash
    let result = (|| {
        let id: i32 = scripting("loadScript", (file.to_string_lossy().as_ref(), &plugin).to_variant())
            .map_err(|e| e.to_string())?
            .child_value(0)
            .get()
            .ok_or("loadScript: bad reply")?;
        call(&format!("/Scripting/Script{id}"), "org.kde.kwin.Script", "run", None).map_err(|e| e.to_string())?;
        let msg = rx.recv_timeout(std::time::Duration::from_millis(1500)).map_err(|_| "no reply from KWin script")?;
        let _ = conn.send_message(&msg.new_method_reply(), gio::DBusSendMessageFlags::NONE);
        let list: String = msg.body().and_then(|b| b.child_value(0).get()).unwrap_or_default();
        Ok(list
            .lines()
            .filter_map(|l| {
                let mut f = l.split(' ');
                let id = f.next()?.to_string();
                let mut n = || f.next()?.parse::<i32>().ok();
                Some(Win { id, x: n()?, y: n()?, w: n()?, h: n()? })
            })
            .filter(|w| w.w > 0 && w.h > 0)
            .collect())
    })();
    conn.remove_filter(filter);
    let _ = scripting("unloadScript", (&plugin,).to_variant());
    let _ = std::fs::remove_file(&file);
    crate::util::trace("windows: listed");
    result
}

const WINDOWS_PATH: &str = "/dev/holyshot/windows";
const WINDOWS_JS: &str = r#"
const d = workspace.currentDesktop, a = workspace.currentActivity, s = workspace.stackingOrder, out = [];
for (let i = s.length - 1; i >= 0; i--) {
    const w = s[i];
    if (!(w.normalWindow || w.dialog || w.utility) || w.minimized || w.hidden) continue;
    if (!w.onAllDesktops && !w.desktops.some(x => x.id == d.id)) continue;
    if (w.activities.length && w.activities.indexOf(a) < 0) continue;
    const g = w.frameGeometry;
    out.push([w.internalId.toString(), Math.round(g.x), Math.round(g.y), Math.round(g.width), Math.round(g.height)].join(" "));
}
callDBus("@ME@", "/dev/holyshot/windows", "dev.holyshot.Windows", "List", out.join("\n"));
"#;
