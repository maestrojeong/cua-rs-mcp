//! The X11 `CLIPBOARD` selection.
//!
//! X11 has no clipboard *store*. "The clipboard" is whichever client currently
//! owns the `CLIPBOARD` selection, and reading it is a request that owner
//! answers. Two consequences shape this module:
//!
//! - **A read is a conversation.** Ask for `TARGETS` (the type list) first,
//!   apply the same withholding policy macOS applies to its pasteboard types
//!   (`cua_core::safety::clipboard_withheld`), and only then ask for the text.
//!   Like macOS, the contents are never pulled into this process when policy
//!   says no.
//! - **A write needs an owner that outlives the call.** `clipboard_set` hands
//!   ownership to a small thread with its own X connection that answers paste
//!   requests until some other client takes the selection over. Clearing the
//!   clipboard is giving the selection up.
//!
//! There is no change counter in X11; `change_count` is reported as `-1`
//! ("unknown") and `verified` is read from `GetSelectionOwner` instead.
//! Selections large enough to need the `INCR` protocol are refused rather than
//! half-read. Wayland-native clipboards are out of reach for the reason given
//! in [`crate::x11`].

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The type marker KDE's Klipper and KeePassXC set on a copied secret: the
/// Linux counterpart of `org.nspasteboard.ConcealedType`.
pub(crate) const CONCEALED: &str = "x-kde-passwordManagerHint";

/// How long a selection owner gets to answer one conversion request.
const CONVERT_TIMEOUT: Duration = Duration::from_millis(1_500);

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    text: Atom,
    incr: Atom,
    prop: Atom,
}

fn intern(conn: &RustConnection, name: &[u8]) -> Result<Atom, String> {
    conn.intern_atom(false, name)
        .map_err(|e| e.to_string())?
        .reply()
        .map(|r| r.atom)
        .map_err(|e| e.to_string())
}

fn atoms(conn: &RustConnection) -> Result<Atoms, String> {
    Ok(Atoms {
        clipboard: intern(conn, b"CLIPBOARD")?,
        targets: intern(conn, b"TARGETS")?,
        utf8: intern(conn, b"UTF8_STRING")?,
        text: intern(conn, b"TEXT")?,
        incr: intern(conn, b"INCR")?,
        prop: intern(conn, b"CUA_RS_SELECTION")?,
    })
}

/// A connection plus an invisible window to receive selection data on.
fn open() -> Result<(RustConnection, Window, Atoms), String> {
    let (conn, screen_num) = x11rb::connect(None).map_err(|e| e.to_string())?;
    let root = conn
        .setup()
        .roots
        .get(screen_num)
        .ok_or("no X screen")?
        .root;
    let win = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .map_err(|e| e.to_string())?;
    let a = atoms(&conn)?;
    conn.flush().map_err(|e| e.to_string())?;
    Ok((conn, win, a))
}

/// Convert the selection to `target` and return the raw property.
fn convert(
    conn: &RustConnection,
    win: Window,
    a: &Atoms,
    target: Atom,
) -> Result<Option<(Atom, Vec<u8>)>, String> {
    conn.convert_selection(win, a.clipboard, target, a.prop, x11rb::CURRENT_TIME)
        .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    let deadline = Instant::now() + CONVERT_TIMEOUT;
    loop {
        match conn.poll_for_event().map_err(|e| e.to_string())? {
            Some(Event::SelectionNotify(ev)) if ev.requestor == win => {
                if ev.property == x11rb::NONE {
                    return Ok(None);
                }
                let reply = conn
                    .get_property(true, win, a.prop, AtomEnum::ANY, 0, u32::MAX / 4)
                    .map_err(|e| e.to_string())?
                    .reply()
                    .map_err(|e| e.to_string())?;
                if reply.type_ == a.incr {
                    return Err(
                        "the clipboard holds more than one X request can carry (INCR transfer), \
                         which cua-rs does not read"
                            .into(),
                    );
                }
                return Ok(Some((reply.type_, reply.value)));
            }
            Some(_) => {}
            None => {
                if Instant::now() > deadline {
                    return Err(format!(
                        "the clipboard owner did not answer within {}ms",
                        CONVERT_TIMEOUT.as_millis()
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn atom_name(conn: &RustConnection, atom: Atom) -> Option<String> {
    conn.get_atom_name(atom)
        .ok()?
        .reply()
        .ok()
        .map(|r| String::from_utf8_lossy(&r.name).into_owned())
}

/// The clipboard's type list. Empty when nobody owns the selection.
pub(crate) fn types() -> Result<Vec<String>, String> {
    let (conn, win, a) = open()?;
    let owner = conn
        .get_selection_owner(a.clipboard)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .owner;
    if owner == x11rb::NONE {
        return Ok(Vec::new());
    }
    let Some((_, raw)) = convert(&conn, win, &a, a.targets)? else {
        return Ok(Vec::new());
    };
    Ok(raw
        .chunks_exact(4)
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .filter_map(|atom| atom_name(&conn, atom))
        .collect())
}

/// The clipboard's text, or `None` when it offers none. Only called after the
/// policy in `clipboard_withheld` has passed on [`types`].
pub(crate) fn text() -> Result<Option<String>, String> {
    let (conn, win, a) = open()?;
    for target in [a.utf8, AtomEnum::STRING.into(), a.text] {
        if let Some((_, raw)) = convert(&conn, win, &a, target)? {
            return Ok(Some(String::from_utf8_lossy(&raw).into_owned()));
        }
    }
    Ok(None)
}

/// Whether `window` (or, for `None`, nobody) owns the selection right now.
fn owner_is(expected: Option<Window>) -> Result<bool, String> {
    let (conn, _win, a) = open()?;
    let owner = conn
        .get_selection_owner(a.clipboard)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .owner;
    Ok(match expected {
        Some(w) => owner == w,
        None => owner == x11rb::NONE,
    })
}

/// Take the selection and serve `text` from a background thread until another
/// client takes it. Returns whether ownership was confirmed.
pub(crate) fn set(text: String) -> Result<bool, String> {
    let (conn, win, a) = open()?;
    conn.set_selection_owner(win, a.clipboard, x11rb::CURRENT_TIME)
        .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    let owned = owner_is(Some(win))?;
    if !owned {
        return Ok(false);
    }
    std::thread::Builder::new()
        .name("cua-clipboard-owner".into())
        .spawn(move || serve(conn, a, text.into_bytes()))
        .map_err(|e| e.to_string())?;
    Ok(true)
}

/// Give the selection up, so nothing is on the clipboard. Returns whether the
/// selection is now unowned.
pub(crate) fn clear() -> Result<bool, String> {
    let (conn, _win, a) = open()?;
    conn.set_selection_owner(x11rb::NONE, a.clipboard, x11rb::CURRENT_TIME)
        .map_err(|e| e.to_string())?;
    conn.flush().map_err(|e| e.to_string())?;
    owner_is(None)
}

/// Answer paste requests until the selection is taken away. The owning window
/// lives exactly as long as `conn`, which this thread holds.
fn serve(conn: RustConnection, a: Atoms, data: Vec<u8>) {
    loop {
        let event = match conn.wait_for_event() {
            Ok(ev) => ev,
            Err(e) => {
                tracing::warn!("clipboard owner lost its X connection: {e}");
                return;
            }
        };
        match event {
            Event::SelectionClear(_) => return,
            Event::SelectionRequest(req) => {
                let mut property = req.property;
                let answered = if req.target == a.targets {
                    let offered = [a.targets, a.utf8, AtomEnum::STRING.into(), a.text];
                    conn.change_property32(
                        PropMode::REPLACE,
                        req.requestor,
                        req.property,
                        AtomEnum::ATOM,
                        &offered,
                    )
                    .is_ok()
                } else if req.target == a.utf8
                    || req.target == a.text
                    || req.target == u32::from(AtomEnum::STRING)
                {
                    conn.change_property8(
                        PropMode::REPLACE,
                        req.requestor,
                        req.property,
                        req.target,
                        &data,
                    )
                    .is_ok()
                } else {
                    false
                };
                if !answered {
                    property = x11rb::NONE;
                }
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property,
                };
                let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
                let _ = conn.flush();
            }
            _ => {}
        }
    }
}
