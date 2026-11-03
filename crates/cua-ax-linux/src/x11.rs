//! Everything AT-SPI does not cover, over the X11 protocol: pixels, displays,
//! window-manager requests, the human-idle reading behind yield-to-human, and
//! the opt-in XTEST input tier.
//!
//! # Why X11 and not Wayland
//!
//! Every one of these is a *global* operation — read another client's pixels,
//! ask which window is active, move someone else's window, inject input — and
//! Wayland's security model removes all of them from ordinary clients on
//! purpose. What replaces them is compositor-specific (`xdg-desktop-portal`
//! screencast sessions that prompt the user, `wlr-*` protocols that GNOME does
//! not implement, `libei` for input). None is a drop-in, and shipping half of
//! them would make behavior depend on the compositor in ways a tool result
//! could not report. So this module is X11 only, and on a Wayland session
//! without an X display each caller answers `PlatformUnsupported` with the
//! reason, rather than appearing to work. AT-SPI itself is display-server
//! agnostic, so the tree and semantic actions work under Wayland regardless.

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, ImageFormat, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use cua_core::{CGPoint, CGRect, CGSize, DisplayInfo, Screenshot};

/// Which display server this session runs, as far as the environment says.
pub(crate) fn session_type() -> &'static str {
    match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => "wayland",
        Ok("x11") => "x11",
        _ if std::env::var_os("WAYLAND_DISPLAY").is_some() => "wayland",
        _ if std::env::var_os("DISPLAY").is_some() => "x11",
        _ => "unknown",
    }
}

/// A live X11 connection and the handful of atoms the EWMH requests use.
pub(crate) struct X11 {
    conn: RustConnection,
    root: Window,
    screen_w: u16,
    screen_h: u16,
    atoms: Atoms,
}

struct Atoms {
    net_client_list: Atom,
    net_wm_pid: Atom,
    net_active_window: Atom,
    net_restack_window: Atom,
    net_moveresize_window: Atom,
    net_wm_state: Atom,
    net_wm_state_max_vert: Atom,
    net_wm_state_max_horz: Atom,
    net_wm_state_hidden: Atom,
    wm_change_state: Atom,
}

/// Why X11 is not available, phrased for a tool result.
pub(crate) fn unavailable_reason(err: &str) -> String {
    if session_type() == "wayland" {
        format!(
            "this is a Wayland session and no X11 display is reachable ({err}). Wayland gives ordinary \
             clients no way to read other windows' pixels, list monitors, move other clients' windows or \
             inject input; cua-rs does not yet speak the compositor-specific portals that replace them"
        )
    } else {
        format!("no X11 display is reachable ({err}); set DISPLAY to the desktop session's display")
    }
}

impl X11 {
    pub(crate) fn connect() -> Result<Self, String> {
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| e.to_string())?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| format!("X server has no screen {screen_num}"))?;
        let (root, screen_w, screen_h) =
            (screen.root, screen.width_in_pixels, screen.height_in_pixels);
        let intern = |name: &[u8]| -> Result<Atom, String> {
            conn.intern_atom(false, name)
                .map_err(|e| e.to_string())?
                .reply()
                .map(|r| r.atom)
                .map_err(|e| e.to_string())
        };
        let atoms = Atoms {
            net_client_list: intern(b"_NET_CLIENT_LIST")?,
            net_wm_pid: intern(b"_NET_WM_PID")?,
            net_active_window: intern(b"_NET_ACTIVE_WINDOW")?,
            net_restack_window: intern(b"_NET_RESTACK_WINDOW")?,
            net_moveresize_window: intern(b"_NET_MOVERESIZE_WINDOW")?,
            net_wm_state: intern(b"_NET_WM_STATE")?,
            net_wm_state_max_vert: intern(b"_NET_WM_STATE_MAXIMIZED_VERT")?,
            net_wm_state_max_horz: intern(b"_NET_WM_STATE_MAXIMIZED_HORZ")?,
            net_wm_state_hidden: intern(b"_NET_WM_STATE_HIDDEN")?,
            wm_change_state: intern(b"WM_CHANGE_STATE")?,
        };
        Ok(Self {
            conn,
            root,
            screen_w,
            screen_h,
            atoms,
        })
    }

    // ── pixels ─────────────────────────────────────────────────────────────

    /// Capture `rect` (global points = X pixels) off the root window.
    ///
    /// A *screen* read, not a window read: X11 only returns a window's own
    /// contents reliably when nothing covers it, and a compositor redirects
    /// windows off-screen entirely. Reading the root returns what the human
    /// sees in that rectangle — including anything on top of the target —
    /// which is the honest picture, and the warning `get_app_state` attaches
    /// says so. X11 has no point/pixel distinction, so `scale` is 1.0.
    pub(crate) fn capture(&self, rect: CGRect, max_dim: u32) -> Result<Screenshot, String> {
        let x0 = rect.origin.x.max(0.0).floor() as i32;
        let y0 = rect.origin.y.max(0.0).floor() as i32;
        let x1 = ((rect.origin.x + rect.size.width).ceil() as i32).min(i32::from(self.screen_w));
        let y1 = ((rect.origin.y + rect.size.height).ceil() as i32).min(i32::from(self.screen_h));
        if x1 <= x0 || y1 <= y0 {
            return Err("the window lies entirely off screen".into());
        }
        let (w, h) = ((x1 - x0) as u16, (y1 - y0) as u16);
        let img = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                x0 as i16,
                y0 as i16,
                w,
                h,
                !0,
            )
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        if img.depth != 24 && img.depth != 32 {
            return Err(format!(
                "unsupported root visual depth {} (only 24/32-bit TrueColor is handled)",
                img.depth
            ));
        }
        let (w, h) = (usize::from(w), usize::from(h));
        if img.data.len() < w * h * 4 {
            return Err("the X server returned a short image".into());
        }
        // 32 bits per pixel, little-endian BGRX on every TrueColor server this
        // targets.
        let mut rgb = Vec::with_capacity(w * h * 3);
        for px in img.data.chunks_exact(4).take(w * h) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        let (rgb, ow, oh) = downscale(rgb, w, h, max_dim as usize);
        let png = encode_png(&rgb, ow as u32, oh as u32)?;
        let frame = CGRect::new(
            CGPoint::new(f64::from(x0), f64::from(y0)),
            CGSize::new(w as f64, h as f64),
        );
        Ok(Screenshot {
            png,
            width: ow as u32,
            height: oh as u32,
            scale: ow as f64 / w as f64,
            frame,
            window_frame: rect,
        })
    }

    /// Active monitors via RandR 1.5, primary first.
    pub(crate) fn monitors(&self) -> Result<Vec<DisplayInfo>, String> {
        let reply = self
            .conn
            .randr_get_monitors(self.root, true)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        let mut out: Vec<DisplayInfo> = reply
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| DisplayInfo {
                id: u32::try_from(i).unwrap_or(u32::MAX),
                frame: CGRect::new(
                    CGPoint::new(f64::from(m.x), f64::from(m.y)),
                    CGSize::new(f64::from(m.width), f64::from(m.height)),
                ),
                main: m.primary,
                scale: 1.0,
            })
            .collect();
        out.sort_by_key(|d| !d.main);
        Ok(out)
    }

    // ── windows ────────────────────────────────────────────────────────────

    fn card32(&self, win: Window, prop: Atom, ty: impl Into<Atom>) -> Option<Vec<u32>> {
        let reply = self
            .conn
            .get_property(false, win, prop, ty, 0, 4096)
            .ok()?
            .reply()
            .ok()?;
        let values: Vec<u32> = reply.value32()?.collect();
        Some(values)
    }

    fn pid_of_window(&self, win: Window) -> Option<libc::pid_t> {
        self.card32(win, self.atoms.net_wm_pid, AtomEnum::CARDINAL)?
            .first()
            .and_then(|p| libc::pid_t::try_from(*p).ok())
    }

    /// A client window's frame in root coordinates (its own geometry, not
    /// counting decorations the window manager drew around it).
    fn frame_of(&self, win: Window) -> Option<CGRect> {
        let geo = self.conn.get_geometry(win).ok()?.reply().ok()?;
        let t = self
            .conn
            .translate_coordinates(win, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some(CGRect::new(
            CGPoint::new(f64::from(t.dst_x), f64::from(t.dst_y)),
            CGSize::new(f64::from(geo.width), f64::from(geo.height)),
        ))
    }

    /// The managed top-level X window belonging to `pid` whose frame is
    /// nearest `frame` — the AT-SPI window to X window pairing, by the same
    /// public-data route macOS uses (pid plus geometry, DESIGN.md §6).
    pub(crate) fn window_for(&self, pid: libc::pid_t, frame: Option<CGRect>) -> Option<Window> {
        let clients = self.card32(self.root, self.atoms.net_client_list, AtomEnum::WINDOW)?;
        let mut best: Option<(f64, Window)> = None;
        for win in clients {
            if self.pid_of_window(win) != Some(pid) {
                continue;
            }
            let d = match (frame, self.frame_of(win)) {
                (Some(a), Some(b)) => {
                    (a.origin.x - b.origin.x).abs()
                        + (a.origin.y - b.origin.y).abs()
                        + (a.size.width - b.size.width).abs()
                        + (a.size.height - b.size.height).abs()
                }
                _ => f64::MAX / 2.0,
            };
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, win));
            }
        }
        best.map(|(_, w)| w)
    }

    /// The pid behind `_NET_ACTIVE_WINDOW`: the app the human is looking at.
    pub(crate) fn active_pid(&self) -> Option<libc::pid_t> {
        let win = *self
            .card32(self.root, self.atoms.net_active_window, AtomEnum::WINDOW)?
            .first()?;
        if win == 0 {
            return None;
        }
        self.pid_of_window(win)
    }

    /// Whether the window manager reports `win` hidden (minimized).
    pub(crate) fn is_hidden(&self, win: Window) -> bool {
        self.card32(win, self.atoms.net_wm_state, AtomEnum::ATOM)
            .is_some_and(|s| s.contains(&self.atoms.net_wm_state_hidden))
    }

    fn client_message(&self, win: Window, ty: Atom, data: [u32; 5]) -> Result<(), String> {
        let ev = ClientMessageEvent::new(32, win, ty, data);
        self.conn
            .send_event(
                false,
                self.root,
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
                ev,
            )
            .map_err(|e| e.to_string())?;
        self.conn.flush().map_err(|e| e.to_string())
    }

    /// Raise without activating: `_NET_RESTACK_WINDOW` (source = pager, stack
    /// mode Above). The window manager decides; the caller reads back.
    pub(crate) fn raise(&self, win: Window) -> Result<(), String> {
        self.client_message(win, self.atoms.net_restack_window, [2, 0, 0, 0, 0])
    }

    /// `_NET_MOVERESIZE_WINDOW` with only the fields given.
    pub(crate) fn move_resize(
        &self,
        win: Window,
        pos: Option<(f64, f64)>,
        size: Option<(f64, f64)>,
    ) -> Result<(), String> {
        // Gravity 0 (window's own), source 2 (pager); bits 8..11 say which of
        // x, y, width, height are present.
        let mut flags: u32 = 2 << 12;
        let (mut x, mut y, mut w, mut h) = (0u32, 0u32, 0u32, 0u32);
        if let Some((px, py)) = pos {
            flags |= (1 << 8) | (1 << 9);
            x = px.round() as i32 as u32;
            y = py.round() as i32 as u32;
        }
        if let Some((sw, sh)) = size {
            flags |= (1 << 10) | (1 << 11);
            w = sw.round().max(1.0) as u32;
            h = sh.round().max(1.0) as u32;
        }
        self.client_message(win, self.atoms.net_moveresize_window, [flags, x, y, w, h])
    }

    /// ICCCM iconify request.
    pub(crate) fn minimize(&self, win: Window) -> Result<(), String> {
        self.client_message(win, self.atoms.wm_change_state, [3, 0, 0, 0, 0])
    }

    /// Add (`true`) or remove both maximized states.
    pub(crate) fn set_maximized(&self, win: Window, on: bool) -> Result<(), String> {
        self.client_message(
            win,
            self.atoms.net_wm_state,
            [
                u32::from(on),
                self.atoms.net_wm_state_max_vert,
                self.atoms.net_wm_state_max_horz,
                2,
                0,
            ],
        )
    }

    /// `_NET_ACTIVE_WINDOW` from a pager: the only EWMH way to de-iconify. It
    /// activates the window — which is why only `restore_window` on a
    /// minimized window uses it.
    pub(crate) fn activate(&self, win: Window) -> Result<(), String> {
        self.client_message(win, self.atoms.net_active_window, [2, 0, 0, 0, 0])
    }

    // ── human idle ─────────────────────────────────────────────────────────

    /// Milliseconds since the last input event on any device, from the
    /// MIT-SCREEN-SAVER extension. A *reading*, not a tap: nothing here sees
    /// what was typed, only when.
    pub(crate) fn idle_ms(&self) -> Option<u64> {
        self.conn
            .screensaver_query_info(self.root)
            .ok()?
            .reply()
            .ok()
            .map(|r| u64::from(r.ms_since_user_input))
    }

    // ── XTEST (opt-in) ─────────────────────────────────────────────────────

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.conn
            .xtest_fake_input(ty, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), String> {
        self.conn
            .get_input_focus()
            .map_err(|e| e.to_string())?
            .reply()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub(crate) fn pointer_move(&self, x: f64, y: f64) -> Result<(), String> {
        self.fake(
            xproto::MOTION_NOTIFY_EVENT,
            0,
            x.round() as i16,
            y.round() as i16,
        )
    }

    /// X button numbers: 1 left, 2 middle, 3 right, 4/5 wheel up/down, 6/7
    /// wheel left/right.
    pub(crate) fn button(&self, button: u8, press: bool) -> Result<(), String> {
        let ty = if press {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(ty, button, 0, 0)
    }

    pub(crate) fn key(&self, keycode: u8, press: bool) -> Result<(), String> {
        let ty = if press {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        self.fake(ty, keycode, 0, 0)
    }

    /// The keycode that produces `keysym`, and whether Shift is needed for it
    /// (it sits in the second column of the core keyboard mapping).
    pub(crate) fn keycode_for(&self, keysym: u32) -> Option<(u8, bool)> {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .ok()?
            .reply()
            .ok()?;
        let per = usize::from(map.keysyms_per_keycode);
        if per == 0 {
            return None;
        }
        for (i, syms) in map.keysyms.chunks(per).enumerate() {
            if let Some(col) = syms.iter().take(2).position(|s| *s == keysym) {
                let code = min.checked_add(u8::try_from(i).ok()?)?;
                return Some((code, col == 1));
            }
        }
        None
    }
}

/// Box-filter `rgb` down so its longer side is at most `max_dim`.
fn downscale(rgb: Vec<u8>, w: usize, h: usize, max_dim: usize) -> (Vec<u8>, usize, usize) {
    let longest = w.max(h);
    if max_dim == 0 || longest <= max_dim {
        return (rgb, w, h);
    }
    let ow = (w * max_dim / longest).max(1);
    let oh = (h * max_dim / longest).max(1);
    let mut out = Vec::with_capacity(ow * oh * 3);
    for oy in 0..oh {
        let (sy0, sy1) = (oy * h / oh, ((oy + 1) * h / oh).max(oy * h / oh + 1));
        for ox in 0..ow {
            let (sx0, sx1) = (ox * w / ow, ((ox + 1) * w / ow).max(ox * w / ow + 1));
            let mut acc = [0u32; 3];
            let mut n = 0u32;
            for sy in sy0..sy1.min(h) {
                for sx in sx0..sx1.min(w) {
                    let p = (sy * w + sx) * 3;
                    acc[0] += u32::from(rgb[p]);
                    acc[1] += u32::from(rgb[p + 1]);
                    acc[2] += u32::from(rgb[p + 2]);
                    n += 1;
                }
            }
            let n = n.max(1);
            out.extend(acc.iter().map(|c| (c / n) as u8));
        }
    }
    (out, ow, oh)
}

fn encode_png(rgb: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgb).map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_keeps_aspect_and_caps_the_long_side() {
        let (out, w, h) = downscale(vec![128; 400 * 200 * 3], 400, 200, 100);
        assert_eq!((w, h), (100, 50));
        assert_eq!(out.len(), 100 * 50 * 3);
        assert!(out.iter().all(|b| *b == 128));
    }

    #[test]
    fn a_small_image_is_left_alone() {
        let (out, w, h) = downscale(vec![1, 2, 3], 1, 1, 1400);
        assert_eq!((w, h, out), (1, 1, vec![1, 2, 3]));
    }

    #[test]
    fn png_encoding_round_trips_its_header() {
        let png = encode_png(&[255, 0, 0, 0, 255, 0], 2, 1).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    }
}
