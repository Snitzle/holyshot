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
}

pub struct Capture {
    pub img: Img,
    /// Output name (e.g. "HDMI-A-1"), only for `Target::Active`.
    pub screen: Option<String>,
    /// Preview-sized copy, when computed ahead of time.
    pub preview: Option<Img>,
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
    opts.insert("include-decoration", false);

    let fds = gio::UnixFDList::new();
    let handle = fds.append(&writer).map_err(|e| e.to_string())?;
    drop(writer);

    let method = match target {
        Target::Workspace => "CaptureWorkspace",
        Target::Active => "CaptureActiveScreen",
    };
    let params = glib::Variant::tuple_from_iter([opts.end(), glib::variant::Handle(handle).to_variant()]);

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
    Ok(Capture { img: Img::from_vec(data, w, h, stride, scale), screen, preview: None })
}
