//! The AT-SPI bus: connecting to it, addressing an object on it, and turning
//! what it says into [`CoreError`]s.
//!
//! AT-SPI2 is a private D-Bus bus, separate from the session bus, whose address
//! the session bus hands out (`org.a11y.Bus.GetAddress`). Every application
//! that has toolkit accessibility enabled registers on it, and every widget is
//! an object on that application's connection: a `(unique bus name, object
//! path)` pair. That pair is the whole of an element handle here — see
//! [`Handle`] — and unlike an `AXUIElement` it is plain data, so it is `Send`
//! and could cross threads. It still does not: all calls go through the one
//! native thread, for the same serialization reason as on macOS (DESIGN.md §4).

use std::future::Future;
use std::time::Duration;

use atspi::proxy::accessible::AccessibleProxy;
use atspi::proxy::action::ActionProxy;
use atspi::proxy::component::ComponentProxy;
use atspi::proxy::editable_text::EditableTextProxy;
use atspi::proxy::text::TextProxy;
use atspi::proxy::value::ValueProxy;
use cua_core::CoreError;
use zbus::proxy::CacheProperties;

/// The per-call ceiling on one D-Bus round trip.
///
/// An AT-SPI call is synchronous IPC into the target application's main loop,
/// exactly like an AX call, and a busy or wedged app simply does not answer.
/// zbus's own default is 25 s, which would stall the whole native thread (and
/// every queued tool call behind it) on one hung app. Two seconds matches
/// `cua_ax::DEFAULT_TIMEOUT_SECS`.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the AT-SPI registry daemon lists every accessible application.
pub(crate) const REGISTRY_BUS: &str = "org.a11y.atspi.Registry";
/// The object path every application uses for its root accessible.
pub(crate) const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";

/// One accessible object: the application connection that owns it and its
/// path on that connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Handle {
    pub(crate) bus: String,
    pub(crate) path: String,
}

impl Handle {
    pub(crate) fn new(bus: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            bus: bus.into(),
            path: path.into(),
        }
    }

    pub(crate) fn from_ref(r: &atspi::ObjectRefOwned) -> Option<Self> {
        if r.is_null() {
            return None;
        }
        let bus = r.name_as_str()?.to_string();
        Some(Self::new(bus, r.path_as_str().to_string()))
    }
}

/// A failed AT-SPI call, before it is given a tool-level meaning.
#[derive(Debug, Clone)]
pub(crate) enum BusFail {
    /// The object, its application or its interface is gone: the widget was
    /// destroyed, the window closed, or the app quit. The Linux counterpart of
    /// `AxError::Stale`.
    Gone(String),
    /// The app did not answer within [`CALL_TIMEOUT`]. The counterpart of
    /// `kAXErrorCannotComplete`.
    Timeout,
    /// Anything else, verbatim.
    Other(String),
}

impl std::fmt::Display for BusFail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusFail::Gone(why) => write!(f, "the element is gone ({why})"),
            BusFail::Timeout => write!(
                f,
                "the app did not answer within {}s (busy, modal, or not responding)",
                CALL_TIMEOUT.as_secs()
            ),
            BusFail::Other(why) => f.write_str(why),
        }
    }
}

impl From<zbus::Error> for BusFail {
    fn from(e: zbus::Error) -> Self {
        BusFail::classify(e.to_string())
    }
}

impl BusFail {
    fn classify(text: String) -> Self {
        // The D-Bus error names a destroyed object, a vanished connection or a
        // missing interface come back as. Matched on the name, which is the
        // stable part; the human-readable tail differs per toolkit.
        const GONE: &[&str] = &[
            "org.freedesktop.DBus.Error.UnknownObject",
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.freedesktop.DBus.Error.UnknownInterface",
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.NameHasNoOwner",
            "org.freedesktop.DBus.Error.NoReply",
        ];
        if GONE.iter().any(|g| text.contains(g)) {
            BusFail::Gone(text)
        } else {
            BusFail::Other(text)
        }
    }
}

impl From<atspi::AtspiError> for BusFail {
    fn from(e: atspi::AtspiError) -> Self {
        match e {
            atspi::AtspiError::Zbus(z) => BusFail::classify(z),
            other => BusFail::Other(other.to_string()),
        }
    }
}

impl From<zbus::fdo::Error> for BusFail {
    fn from(e: zbus::fdo::Error) -> Self {
        BusFail::from(zbus::Error::from(e))
    }
}

impl From<BusFail> for CoreError {
    fn from(e: BusFail) -> Self {
        CoreError::Accessibility {
            reason: e.to_string(),
        }
    }
}

pub(crate) type BusResult<T> = std::result::Result<T, BusFail>;

/// Await `fut` for at most [`CALL_TIMEOUT`].
pub(crate) async fn timed<T, E, F>(fut: F) -> BusResult<T>
where
    F: Future<Output = std::result::Result<T, E>>,
    E: Into<BusFail>,
{
    match tokio::time::timeout(CALL_TIMEOUT, fut).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => Err(BusFail::Timeout),
    }
}

/// Build an interface proxy for `$handle` on `$conn`, uncached: every property
/// read is a fresh round trip, because a cached property is exactly the stale
/// read this project refuses to act on.
macro_rules! proxy {
    ($ty:ident, $conn:expr, $handle:expr) => {
        async {
            let h: &$crate::bus::Handle = $handle;
            $ty::builder($conn)
                .destination(h.bus.clone())
                .and_then(|b| b.path(h.path.clone()))
                .map(|b| b.cache_properties(CacheProperties::No))
                .map_err($crate::bus::BusFail::from)?
                .build()
                .await
                .map_err($crate::bus::BusFail::from)
        }
    };
}

pub(crate) async fn accessible(
    conn: &zbus::Connection,
    h: &Handle,
) -> BusResult<AccessibleProxy<'static>> {
    proxy!(AccessibleProxy, conn, h).await
}

pub(crate) async fn component(
    conn: &zbus::Connection,
    h: &Handle,
) -> BusResult<ComponentProxy<'static>> {
    proxy!(ComponentProxy, conn, h).await
}

pub(crate) async fn action(conn: &zbus::Connection, h: &Handle) -> BusResult<ActionProxy<'static>> {
    proxy!(ActionProxy, conn, h).await
}

pub(crate) async fn text(conn: &zbus::Connection, h: &Handle) -> BusResult<TextProxy<'static>> {
    proxy!(TextProxy, conn, h).await
}

pub(crate) async fn editable_text(
    conn: &zbus::Connection,
    h: &Handle,
) -> BusResult<EditableTextProxy<'static>> {
    proxy!(EditableTextProxy, conn, h).await
}

pub(crate) async fn value(conn: &zbus::Connection, h: &Handle) -> BusResult<ValueProxy<'static>> {
    proxy!(ValueProxy, conn, h).await
}

/// Connect to the accessibility bus. `Err` carries why, for
/// `check_permissions` and for the error every other call returns.
pub(crate) async fn connect() -> std::result::Result<zbus::Connection, String> {
    match tokio::time::timeout(
        Duration::from_secs(5),
        atspi::AccessibilityConnection::new(),
    )
    .await
    {
        Ok(Ok(c)) => Ok(c.connection().clone()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("timed out after 5s asking the session bus for the AT-SPI bus".into()),
    }
}

/// The pid behind a bus connection, as the bus daemon itself reports it — the
/// one pid an app cannot misreport about itself.
pub(crate) async fn pid_of(conn: &zbus::Connection, bus: &str) -> BusResult<libc::pid_t> {
    let dbus = timed(zbus::fdo::DBusProxy::new(conn)).await?;
    let name = zbus::names::BusName::try_from(bus.to_string())
        .map_err(|e| BusFail::Other(e.to_string()))?;
    let pid: u32 = timed(dbus.get_connection_unix_process_id(name)).await?;
    libc::pid_t::try_from(pid).map_err(|_| BusFail::Other(format!("pid {pid} out of range")))
}
