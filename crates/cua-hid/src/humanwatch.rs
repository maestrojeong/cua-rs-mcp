//! A listen-only tap that notices the human is at the keyboard.
//!
//! # Why this lives in `cua-hid`
//!
//! Every other crate in the workspace is free of `CGEvent`, and that is not a
//! tidiness preference — it is how a reader checks the product's central claim.
//! "Only `cua-hid` touches the event APIs" is verifiable by looking at
//! `Cargo.toml` files, whereas "the tap only reads, honestly" is a promise about
//! code somebody has to go read. The policy that decides *whether* to watch, and
//! what a refusal says, stays in `cua-core::safety`; the `CGEvent` surface stays
//! here, next to the synthesis it is the counterpart of.
//!
//! # Why a tap does not break the promise
//!
//! The tap is `kCGEventTapOptionListenOnly` and its callback returns every event
//! unchanged. A listen-only tap is not in the delivery path: it cannot swallow,
//! delay, rewrite or reorder what the human typed. What this crate refuses to do
//! is *write* to the shared cursor and keyboard — that is what steals focus and
//! makes an agent contend with a person for one channel — and reading a stream
//! is not that.
//!
//! It is still a change in what the process does, which is why `cua-core` keeps
//! it behind an opt-in flag rather than starting it by default.
//!
//! # What it records
//!
//! A timestamp, and nothing else. Not the key, not the position, not the app.
//! "Is the human working in the app I am about to drive" is answered by the
//! caller, which pairs this timestamp with the frontmost pid on the thread that
//! is already asking. Keeping the callback to one atomic store means it
//! allocates nothing, takes no lock, and calls into no framework while sitting
//! on the session's input path.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Nanoseconds (`CLOCK_MONOTONIC`) of the last real input event seen.
///
/// A process-global atomic rather than something the watcher owns, because the
/// tap callback is a bare `extern "C"` function pointer with no captured state
/// and this is the smallest thing it can safely touch.
static LAST_HUMAN_INPUT: AtomicU64 = AtomicU64::new(0);

/// Set by the callback when macOS disables the tap; cleared by the watcher
/// thread after re-enabling it.
static TAP_NEEDS_REENABLE: AtomicBool = AtomicBool::new(false);

fn now_nanos() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, correctly typed timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// A running listen-only input tap and the thread that pumps it.
///
/// Dropping it tears the tap down. There is no way to construct one without
/// starting a tap, so a value of this type is itself the evidence that the watch
/// is up — which is what lets the caller model "flag set but tap unavailable" as
/// a distinct, fail-closed state instead of a silent nothing.
pub struct InputWatch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl InputWatch {
    /// Create the tap and wait for it to report.
    ///
    /// The tap either exists or it does not, and the answer arrives in
    /// microseconds, so this blocks for it: a caller that returned optimistically
    /// would let the first action read a stale "fine".
    ///
    /// `Err` carries a reason written for whoever has to fix it — in practice
    /// always a missing grant.
    pub fn start() -> std::result::Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<std::result::Result<(), String>>();
        let thread_stop = stop.clone();

        let thread = std::thread::Builder::new()
            .name("cua-human-watch".into())
            .spawn(move || run_watch_loop(&thread_stop, &ready_tx))
            .map_err(|e| e.to_string())?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(reason)) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err(reason)
            }
            Err(_) => Err("the watcher thread exited before reporting".to_string()),
        }
    }

    /// Milliseconds since the last human input event, or `None` if none has been
    /// seen since the tap came up.
    pub fn since_input_ms(&self) -> Option<u64> {
        let last = LAST_HUMAN_INPUT.load(Ordering::Relaxed);
        if last == 0 {
            return None;
        }
        Some(now_nanos().saturating_sub(last) / 1_000_000)
    }
}

impl Drop for InputWatch {
    /// Tear the tap down.
    ///
    /// The thread polls its run loop in short slices rather than blocking in
    /// `CFRunLoopRun`, so stopping it is a flag plus a join and needs no
    /// cross-thread `CFRunLoopStop` against a `!Send` run-loop handle. The join
    /// matters: the `CFMachPort` must be invalidated on the thread that created
    /// it, before the process tears down around it.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The tap callback. Returns its event untouched, always.
///
/// # Safety
///
/// Matches `CGEventTapCallBack`. Touches nothing but two atomics, so it is safe
/// to run on the session's input path.
unsafe extern "C-unwind" fn tap_callback(
    _proxy: objc2_core_graphics::CGEventTapProxy,
    event_type: objc2_core_graphics::CGEventType,
    event: std::ptr::NonNull<objc2_core_graphics::CGEvent>,
    _user_info: *mut std::ffi::c_void,
) -> *mut objc2_core_graphics::CGEvent {
    // macOS disables a tap that has been unresponsive, and tells it so through
    // these two pseudo-events. A listen-only tap should never see the timeout
    // one, but a tap that has been silently switched off is a yield gate that
    // has silently stopped working, so it is handled rather than assumed away.
    const DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
    const DISABLED_BY_USER: u32 = 0xFFFF_FFFF;
    if event_type.0 == DISABLED_BY_TIMEOUT || event_type.0 == DISABLED_BY_USER {
        TAP_NEEDS_REENABLE.store(true, Ordering::Relaxed);
    } else {
        LAST_HUMAN_INPUT.store(now_nanos(), Ordering::Relaxed);
    }
    // Unchanged, and not consumed. This is the whole reason a listen-only tap
    // is compatible with this crate's promise.
    event.as_ptr()
}

/// Which events mean a person is present.
///
/// Mouse *movement* is excluded deliberately: a cursor nudged by a trackpad
/// brush is not somebody taking over a window, and including it would make the
/// gate fire on almost any desk.
///
/// `kCGEventLeftMouseDown`/`Up`, `RightMouseDown`/`Up`, `KeyDown`/`Up`,
/// `FlagsChanged`, `ScrollWheel`, `OtherMouseDown`.
const HUMAN_EVENT_TYPES: &[u32] = &[1, 2, 3, 4, 10, 11, 12, 22, 25];

fn mask_for(types: &[u32]) -> u64 {
    types.iter().fold(0u64, |m, t| m | (1u64 << t))
}

/// Own the tap and pump its run loop until asked to stop.
fn run_watch_loop(
    stop: &AtomicBool,
    ready: &std::sync::mpsc::Sender<std::result::Result<(), String>>,
) {
    use objc2_core_foundation::{kCFRunLoopDefaultMode, CFMachPort, CFRunLoop};
    use objc2_core_graphics::{
        CGEvent, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    };

    let mask = mask_for(HUMAN_EVENT_TYPES);

    // SAFETY: `tap_callback` has the required signature and touches no user
    // info, so a null `user_info` is correct.
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::AnnotatedSessionEventTap,
            CGEventTapPlacement::TailAppendEventTap,
            CGEventTapOptions::ListenOnly,
            mask,
            Some(tap_callback),
            std::ptr::null_mut(),
        )
    };

    let Some(port) = port else {
        let _ = ready.send(Err(
            "CGEventTapCreate returned nothing, which on macOS means the process that launched \
             cua-rs holds neither Accessibility nor Input Monitoring"
                .to_string(),
        ));
        return;
    };

    let Some(source) = CFMachPort::new_run_loop_source(None, Some(&port), 0) else {
        let _ = ready.send(Err(
            "the input tap could not be attached to a run loop".to_string()
        ));
        port.invalidate();
        return;
    };

    let Some(run_loop) = CFRunLoop::current() else {
        let _ = ready.send(Err("this thread has no run loop".to_string()));
        port.invalidate();
        return;
    };
    // SAFETY: reading the framework's own mode constant.
    let mode = unsafe { kCFRunLoopDefaultMode };
    run_loop.add_source(Some(&source), mode);
    CGEvent::tap_enable(&port, true);

    let _ = ready.send(Ok(()));

    // Short slices rather than one blocking `CFRunLoopRun`, so teardown is a
    // flag rather than a cross-thread stop against a `!Send` handle. 250 ms is
    // the whole cost of shutting the server down.
    while !stop.load(Ordering::Relaxed) {
        CFRunLoop::run_in_mode(mode, 0.25, false);
        if TAP_NEEDS_REENABLE.swap(false, Ordering::Relaxed) {
            tracing::warn!("the yield-to-human input tap was disabled by macOS; re-enabling");
            CGEvent::tap_enable(&port, true);
        }
    }

    run_loop.remove_source(Some(&source), mode);
    CGEvent::tap_enable(&port, false);
    port.invalidate();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_mask_names_presses_and_not_movement() {
        let mask = mask_for(HUMAN_EVENT_TYPES);
        // Every type the watch subscribes to is set.
        for t in HUMAN_EVENT_TYPES {
            assert_ne!(mask & (1u64 << t), 0, "type {t} should be in the mask");
        }
        // kCGEventMouseMoved (5) and the three drag types (6, 7, 27) are not:
        // a brushed trackpad is not somebody taking over a window.
        for t in [5u32, 6, 7, 27] {
            assert_eq!(mask & (1u64 << t), 0, "type {t} must not be in the mask");
        }
    }

    #[test]
    fn no_input_seen_reads_as_unknown_rather_than_zero_ago() {
        // The static starts at 0, which has to mean "nothing seen yet" and not
        // "input arrived at the epoch, i.e. an eternity ago" — the latter would
        // read as a permanently idle human and quietly disable the gate.
        assert_eq!(LAST_HUMAN_INPUT.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_monotonic_clock_advances() {
        let a = now_nanos();
        assert_ne!(a, 0, "CLOCK_MONOTONIC should be readable");
        let b = now_nanos();
        assert!(b >= a, "a monotonic clock must not go backwards");
    }
}
