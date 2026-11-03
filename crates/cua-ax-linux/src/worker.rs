//! The native thread, its state, and the async-facing handle.
//!
//! Shaped exactly like the macOS worker (`cua_core::session::worker`): one
//! long-lived thread owns every connection and the snapshot store, jobs are
//! closures sent over a channel, and a panic inside a job becomes that job's
//! [`CoreError::NativePanic`] instead of taking the thread down. The one
//! difference is inside the thread: AT-SPI is D-Bus and zbus is async, so the
//! thread owns a current-thread tokio runtime and `block_on`s each call. That
//! runtime never leaves the thread and never meets the MCP server's runtime.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use cua_core::safety::{Candidate, ContextNode, Gate, SessionProbe, YieldReading};
use cua_core::session::Result;
use cua_core::snapshot::render_tree;
use cua_core::{AppInfo, CGRect, CoreError, Limits, Observed, PostActionState, Target};

use crate::bus::{self, Handle};
use crate::tree::{self, BusApp, LinuxNode};
use crate::x11::{self, X11};

type Job = Box<dyn FnOnce(&mut Inner) + Send>;
pub(crate) type OpFn<T> = Box<dyn FnOnce(&mut Inner) -> Result<T> + Send>;

/// The platform name every `PlatformUnsupported` from this backend carries.
pub(crate) const PLATFORM: &str = "linux";

/// Handle to the Linux native thread. Cheap to clone, safe to share.
#[derive(Clone)]
pub struct LinuxCua {
    tx: mpsc::Sender<Job>,
}

/// One prepared operation for the Linux native thread. See
/// `cua_core::PlatformAdapter::Op`.
pub struct Op<T> {
    pub(crate) run: OpFn<T>,
}

impl<T: Send + 'static> Op<T> {
    pub(crate) fn new<F>(f: F) -> Self
    where
        F: FnOnce(&mut Inner) -> Result<T> + Send + 'static,
    {
        Self { run: Box::new(f) }
    }
}

impl LinuxCua {
    /// Spawn the native thread. Connections are made lazily, on the first call
    /// that needs each one, so a server started before the desktop session is
    /// fully up still comes up and `check_permissions` can say what is missing.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("cua-native".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        // Without a runtime no job can run. Dropping `rx` makes
                        // every call answer `WorkerGone` rather than hang.
                        tracing::error!("cua-native: cannot build a tokio runtime: {e}");
                        return;
                    }
                };
                let mut inner = Inner::new(rt);
                while let Ok(job) = rx.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job(&mut inner);
                    }));
                    if let Err(e) = result {
                        tracing::error!("native job panicked: {e:?}");
                    }
                }
            });
        if let Err(e) = spawned {
            // Same outcome as a thread that died: `tx`'s receiver is gone, so
            // every call reports WorkerGone. No panic at construction.
            tracing::error!("cannot spawn the cua-native thread: {e}");
        }
        Self { tx }
    }

    /// Run `f` on the native thread and wait for its result.
    pub(crate) fn exec<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Inner) -> T + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Box::new(move |inner| {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(inner)))
                    .map_err(|_| CoreError::NativePanic);
                let _ = tx.send(outcome);
            }))
            .map_err(|_| CoreError::WorkerGone)?;
        rx.recv().map_err(|_| CoreError::WorkerGone)?
    }
}

impl Default for LinuxCua {
    fn default() -> Self {
        Self::new()
    }
}

/// Monotonic snapshot ids, process-wide, for the same reason as on macOS: an
/// id is never ambiguous across apps.
static NEXT_SNAPSHOT_ID: AtomicU64 = AtomicU64::new(1);

/// One tree walk of one window.
pub(crate) struct Snapshot {
    pub(crate) id: u64,
    pub(crate) app: BusApp,
    /// Process incarnation, see [`tree::process_start`].
    pub(crate) start: u64,
    pub(crate) nodes: Vec<LinuxNode>,
    pub(crate) tree: String,
    pub(crate) window: Option<Handle>,
    pub(crate) x_window: Option<u32>,
    pub(crate) frame: Option<CGRect>,
    pub(crate) limits: Limits,
    pub(crate) complete: bool,
    pub(crate) scoped: bool,
    /// An action ran since this walk and nothing re-read the window. Same
    /// meaning as on macOS: indices still resolve, coordinates do not.
    pub(crate) acted_on: bool,
}

pub(crate) fn next_snapshot_id() -> u64 {
    NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) struct Inner {
    pub(crate) rt: tokio::runtime::Runtime,
    a11y: Option<zbus::Connection>,
    session_bus: Option<zbus::Connection>,
    x11: Option<X11>,
    pub(crate) snapshots: HashMap<libc::pid_t, Snapshot>,
    /// `CUA_LINUX_XTEST=1`: the operator accepts shared-device input.
    pub(crate) xtest: bool,
}

impl Inner {
    fn new(rt: tokio::runtime::Runtime) -> Self {
        Self {
            rt,
            a11y: None,
            session_bus: None,
            x11: None,
            snapshots: HashMap::new(),
            xtest: cua_core::safety::truthy(
                std::env::var("CUA_LINUX_XTEST").as_deref().unwrap_or(""),
            ),
        }
    }

    // ── connections ────────────────────────────────────────────────────────

    /// The accessibility bus, connecting on first use. Not cached on failure,
    /// so a session that enables accessibility later is picked up.
    pub(crate) fn a11y(&mut self) -> Result<zbus::Connection> {
        if let Some(c) = &self.a11y {
            return Ok(c.clone());
        }
        match self.rt.block_on(bus::connect()) {
            Ok(c) => {
                self.a11y = Some(c.clone());
                Ok(c)
            }
            Err(why) => Err(CoreError::PlatformUnsupported {
                feature: "accessibility",
                platform: PLATFORM,
                why: format!(
                    "the AT-SPI accessibility bus is not reachable ({why}). Install at-spi2-core and run \
                     cua-rs inside the desktop session; see check_permissions"
                ),
            }),
        }
    }

    /// The X11 connection, connecting on first use; `feature` names what
    /// needed it, for the error.
    pub(crate) fn x11(&mut self, feature: &'static str) -> Result<&X11> {
        if self.x11.is_none() {
            match X11::connect() {
                Ok(x) => self.x11 = Some(x),
                Err(e) => {
                    return Err(CoreError::PlatformUnsupported {
                        feature,
                        platform: PLATFORM,
                        why: x11::unavailable_reason(&e),
                    })
                }
            }
        }
        self.x11.as_ref().ok_or(CoreError::WorkerGone)
    }

    /// The X11 connection if one is already up; call [`Inner::x11`] first.
    pub(crate) fn x11_ref(&self) -> Option<&X11> {
        self.x11.as_ref()
    }

    pub(crate) fn x11_available(&mut self) -> bool {
        self.x11("display").is_ok()
    }

    /// Run one async AT-SPI job on this thread's runtime.
    pub(crate) fn block<F: std::future::Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    // ── apps ───────────────────────────────────────────────────────────────

    pub(crate) fn bus_apps(&mut self) -> Result<Vec<BusApp>> {
        let c = self.a11y()?;
        Ok(self.block(tree::list_apps(&c))?)
    }

    /// `query` to exactly one app, with macOS's matching tiers.
    pub(crate) fn resolve(&mut self, query: &str) -> Result<BusApp> {
        let apps = self.bus_apps()?;
        let infos: Vec<AppInfo> = apps.iter().map(|a| a.info.clone()).collect();
        let info = cua_core::apps::resolve_in(query, &infos)?;
        apps.into_iter()
            .find(|a| a.info.pid == info.pid)
            .ok_or_else(|| {
                CoreError::Resolve(cua_core::ResolveError::NotFound {
                    query: query.to_string(),
                })
            })
    }

    // ── safety ─────────────────────────────────────────────────────────────

    /// Read the session facts the shared gate needs, now.
    pub(crate) fn probe(&mut self) -> LinuxProbe {
        let locked = self.screen_locked();
        let yield_on = cua_core::safety::yield_to_human_enabled();
        let (idle, frontmost) = match self.x11("yield_to_human") {
            Ok(x) => (x.idle_ms(), x.active_pid()),
            Err(_) => (None, None),
        };
        let watch = if !yield_on {
            WatchState::Off
        } else if let Some(ms) = idle {
            WatchState::Watching(Some(ms))
        } else {
            // Fails closed, as the macOS tap does when it cannot be created.
            WatchState::Broken(
                "CUA_YIELD_TO_HUMAN needs the X11 MIT-SCREEN-SAVER idle reading, which is not \
                 available in this session (no X display, or a Wayland session)"
                    .to_string(),
            )
        };
        LinuxProbe {
            locked,
            watch,
            frontmost,
        }
    }

    /// Whether the session's screen saver / locker is active, from the
    /// freedesktop (KDE, Xfce, …) or GNOME screen-saver service on the session
    /// bus. Fails *open*, like macOS: a session with no such service (a bare
    /// X server, CI under Xvfb) must not read as permanently locked.
    fn screen_locked(&mut self) -> bool {
        if self.session_bus.is_none() {
            let connected = self.block(async {
                tokio::time::timeout(bus::CALL_TIMEOUT, zbus::Connection::session()).await
            });
            if let Ok(Ok(c)) = connected {
                self.session_bus = Some(c);
            }
        }
        let Some(conn) = self.session_bus.clone() else {
            return false;
        };
        const SERVICES: &[(&str, &str, &str)] = &[
            (
                "org.gnome.ScreenSaver",
                "/org/gnome/ScreenSaver",
                "org.gnome.ScreenSaver",
            ),
            (
                "org.freedesktop.ScreenSaver",
                "/org/freedesktop/ScreenSaver",
                "org.freedesktop.ScreenSaver",
            ),
        ];
        self.block(async {
            for (dest, path, iface) in SERVICES {
                let call = conn.call_method(Some(*dest), *path, Some(*iface), "GetActive", &());
                if let Ok(Ok(reply)) = tokio::time::timeout(bus::CALL_TIMEOUT, call).await {
                    if let Ok(active) = reply.body().deserialize::<bool>() {
                        return active;
                    }
                }
            }
            false
        })
    }

    /// Every gate, in the shared order (`cua_core::safety::guard_with`).
    pub(crate) fn guard(
        &mut self,
        app: &AppInfo,
        gate: &Gate,
        candidate: Option<&Candidate>,
    ) -> Result<()> {
        let probe = self.probe();
        cua_core::safety::guard_with(app, &probe, gate, candidate).map_err(CoreError::Refused)
    }

    /// Lock and yield only, for calls with no app (clipboard).
    pub(crate) fn guard_session(&mut self, verb: &'static str) -> Result<()> {
        let probe = self.probe();
        cua_core::safety::session_gate(
            verb,
            probe.locked,
            probe.yield_reading(),
            cua_core::safety::yield_idle_ms(),
        )
        .map_err(CoreError::Refused)
    }

    // ── snapshots and targets ──────────────────────────────────────────────

    /// The current snapshot for `app`, checked to belong to the same process
    /// incarnation.
    pub(crate) fn current(&self, app: &BusApp, query: &str) -> Result<&Snapshot> {
        let snap = self
            .snapshots
            .get(&app.info.pid)
            .ok_or_else(|| CoreError::NoSnapshot {
                app: query.to_string(),
            })?;
        if snap.app.root.bus != app.root.bus || snap.start != tree::process_start(app.info.pid) {
            return Err(CoreError::ProcessReplaced {
                app: query.to_string(),
            });
        }
        Ok(snap)
    }

    /// Resolve a [`Target`] against the current snapshot, with the same
    /// staleness rules — and the same errors — as the macOS backend.
    pub(crate) fn locate(&self, app: &BusApp, query: &str, target: &Target) -> Result<usize> {
        let snap = self.current(app, query)?;
        match target {
            Target::Index {
                index,
                snapshot_id,
                expected_role,
            } => {
                if let Some(given) = snapshot_id {
                    if *given != snap.id {
                        return Err(CoreError::StaleSnapshot {
                            index: *index,
                            given: *given,
                            current: snap.id,
                        });
                    }
                }
                let node = snap.nodes.get(*index).ok_or(CoreError::BadIndex {
                    index: *index,
                    count: snap.nodes.len(),
                })?;
                if let Some(want) = expected_role {
                    if !want.eq_ignore_ascii_case(&node.role) {
                        return Err(CoreError::TokenRoleMismatch {
                            index: *index,
                            expected: want.clone(),
                            found: node.role.clone(),
                        });
                    }
                }
                Ok(*index)
            }
            Target::Point { x, y, snapshot_id } => {
                if let Some(given) = snapshot_id {
                    if *given != snap.id {
                        return Err(CoreError::StaleCoordinate {
                            app: query.to_string(),
                            given: *given,
                            current: snap.id,
                            x: f64::from(*x),
                            y: f64::from(*y),
                        });
                    }
                }
                if snap.acted_on {
                    return Err(CoreError::StalePointGeometry {
                        app: query.to_string(),
                    });
                }
                hit_test(&snap.nodes, f64::from(*x), f64::from(*y)).ok_or(
                    CoreError::NoElementAtPoint {
                        app: query.to_string(),
                        x: *x,
                        y: *y,
                    },
                )
            }
        }
    }

    /// The destructive-label candidate for node `index`, with its decision
    /// context, built exactly as the macOS targeting code builds it.
    pub(crate) fn candidate(snap: &Snapshot, index: usize) -> Option<Candidate> {
        let node = snap.nodes.get(index)?;
        let projection: Vec<ContextNode<'_>> = snap
            .nodes
            .iter()
            .map(|n| ContextNode {
                parent: n.parent,
                role: &n.role,
                subrole: n.subrole.as_deref(),
                label: n.label.as_deref(),
                value: n.value.as_deref(),
                help: n.help.as_deref(),
                settable: n.settable,
            })
            .collect();
        Some(Candidate {
            role: node.role.clone(),
            label: node.label.clone(),
            value: node.value.clone(),
            help: node.help.clone(),
            settable: node.settable,
            caption: cua_core::safety::caption(&projection, index),
            description: node.describe(),
            context: cua_core::safety::decision_context(&projection, index),
        })
    }

    /// Re-read the element before acting on it: gone, or now showing something
    /// else, is the snapshot drift the macOS backend also refuses on.
    pub(crate) fn revalidate(&mut self, pid: libc::pid_t, index: usize) -> Result<()> {
        let c = self.a11y()?;
        let Some(snap) = self.snapshots.get(&pid) else {
            return Ok(());
        };
        let Some(node) = snap.nodes.get(index) else {
            return Ok(());
        };
        let (handle, label, desc) = (node.handle.clone(), node.label.clone(), node.describe());
        let live = self.block(tree::live_identity(&c, &handle));
        match live {
            Ok((_, live_label)) => {
                // A label that was there and changed is a recycled row; one
                // that was absent and appeared is not evidence of anything.
                if label.is_some() && live_label.is_some() && live_label != label {
                    return Err(CoreError::TargetChanged {
                        index,
                        expected: format!("{:?}", label.unwrap_or_default()),
                        found: format!("{:?}", live_label.unwrap_or_default()),
                    });
                }
                Ok(())
            }
            Err(e) => Err(tree::gone_or(e, index, &desc)),
        }
    }

    /// After an action: re-read the window if asked, and say whether the UI
    /// changed. Without a re-read the honest answer is `Unknown`, and the
    /// snapshot is marked so coordinates against it are refused.
    pub(crate) fn settle(
        &mut self,
        pid: libc::pid_t,
        return_state: bool,
    ) -> (Observed, Option<PostActionState>) {
        if !return_state {
            if let Some(s) = self.snapshots.get_mut(&pid) {
                s.acted_on = true;
            }
            return (Observed::Unknown, None);
        }
        // Give the app's main loop a moment to process what it was just sent.
        std::thread::sleep(std::time::Duration::from_millis(150));
        let Some(prev) = self.snapshots.get(&pid) else {
            return (Observed::Unknown, None);
        };
        let (before, app, window, complete, scoped, limits) = (
            prev.tree.clone(),
            prev.app.clone(),
            prev.window.clone(),
            prev.complete,
            prev.scoped,
            prev.limits,
        );
        let c = match self.a11y() {
            Ok(c) => c,
            Err(e) => {
                return (
                    Observed::Unknown,
                    Some(PostActionState {
                        snapshot_id: None,
                        diff: None,
                        note: Some(format!("could not re-read the window: {e}")),
                        node_count: 0,
                    }),
                )
            }
        };
        let root = match window {
            Some(w) => w,
            None => return (Observed::Unknown, None),
        };
        let walked = self.block(tree::walk(&c, root.clone(), limits, self.xtest));
        let walk = match walked {
            Ok(w) => w,
            Err(e) => {
                return (
                    Observed::Unknown,
                    Some(PostActionState {
                        snapshot_id: None,
                        diff: None,
                        note: Some(format!(
                            "the window could not be re-read after the action ({e}); it may have closed"
                        )),
                        node_count: 0,
                    }),
                )
            }
        };
        let render = cua_core::RenderOptions {
            note_omissions: false,
            ..cua_core::RenderOptions::default()
        };
        let after = render_tree(&walk.nodes, render);
        let changed = if after == before {
            Observed::Unchanged
        } else {
            Observed::Changed
        };
        let (diff, note) = if scoped {
            (
                None,
                Some("the previous snapshot was scoped to one subtree, so no diff is given".into()),
            )
        } else if !complete || !walk.complete {
            (
                None,
                Some("a walk hit a cap or its time budget, so a diff would report unreached nodes as changes".into()),
            )
        } else {
            (Some(cua_core::diff_trees(&before, &after)), None)
        };
        let id = next_snapshot_id();
        let node_count = walk.nodes.len();
        let start = tree::process_start(pid);
        let (x_window, frame) = self
            .snapshots
            .get(&pid)
            .map(|s| (s.x_window, s.frame))
            .unwrap_or((None, None));
        self.snapshots.insert(
            pid,
            Snapshot {
                id,
                app,
                start,
                nodes: walk.nodes,
                tree: after,
                window: Some(root),
                x_window,
                frame,
                limits,
                complete: walk.complete,
                scoped: false,
                acted_on: false,
            },
        );
        (
            changed,
            Some(PostActionState {
                snapshot_id: Some(id),
                diff,
                note,
                node_count,
            }),
        )
    }
}

/// The smallest node whose frame contains the point, preferring actionable
/// ones — a click on a label inside a button means the button.
fn hit_test(nodes: &[LinuxNode], x: f64, y: f64) -> Option<usize> {
    use cua_core::snapshot::TreeNode;
    let contains = |f: &CGRect| {
        x >= f.origin.x
            && y >= f.origin.y
            && x < f.origin.x + f.size.width
            && y < f.origin.y + f.size.height
    };
    let area = |f: &CGRect| f.size.width * f.size.height;
    nodes
        .iter()
        .filter(|n| n.frame.as_ref().is_some_and(contains))
        .min_by(|a, b| {
            let key = |n: &LinuxNode| {
                (
                    !n.is_actionable(),
                    n.frame.as_ref().map(area).unwrap_or(f64::MAX),
                )
            };
            let (ka, kb) = (key(a), key(b));
            ka.0.cmp(&kb.0)
                .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
        })
        .map(|n| n.index)
}

// ── session probe ────────────────────────────────────────────────────────────

pub(crate) enum WatchState {
    Off,
    Broken(String),
    Watching(Option<u64>),
}

/// The Linux answers to the questions the shared gate asks.
pub(crate) struct LinuxProbe {
    pub(crate) locked: bool,
    pub(crate) watch: WatchState,
    pub(crate) frontmost: Option<libc::pid_t>,
}

impl SessionProbe for LinuxProbe {
    fn session_locked(&self) -> bool {
        self.locked
    }
    fn yield_reading(&self) -> YieldReading<'_> {
        match &self.watch {
            WatchState::Off => YieldReading::Off,
            WatchState::Broken(why) => YieldReading::Broken(why),
            WatchState::Watching(ms) => YieldReading::Watching {
                since_input_ms: *ms,
            },
        }
    }
    fn frontmost_pid(&self) -> Option<libc::pid_t> {
        self.frontmost
    }
}

/// `PlatformUnsupported` for `feature`, with `why`.
pub(crate) fn unsupported(feature: &'static str, why: impl Into<String>) -> CoreError {
    CoreError::PlatformUnsupported {
        feature,
        platform: PLATFORM,
        why: why.into(),
    }
}
