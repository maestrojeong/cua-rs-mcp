//! Finding the app the agent asked for.

use std::sync::mpsc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication,
    NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{NSBundle, NSError, NSString, NSURL};

/// A running application, as an agent should see it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: Option<String>,
    pub pid: libc::pid_t,
    /// Whether this app is currently frontmost.
    pub active: bool,
    /// Whether the app has a Dock icon and can show windows. Background agents,
    /// XPC services and menu-bar-only helpers are `false` and are almost never
    /// what a user means by an app name.
    pub regular: bool,
}

/// Every running application with a GUI presence.
///
/// Ordered so the frontmost app comes first: when a user says "the browser" or
/// gives an ambiguous name, what they are looking at is the best prior.
pub fn list_apps() -> Vec<AppInfo> {
    let workspace = NSWorkspace::sharedWorkspace();
    let running = workspace.runningApplications();

    let mut out: Vec<AppInfo> = running
        .iter()
        .map(|app| app_info(&app))
        // Processes with no GUI at all cannot be driven and only add noise.
        .filter(|a| a.pid > 0)
        .collect();

    sort_apps(&mut out);
    out
}

fn app_info(app: &NSRunningApplication) -> AppInfo {
    AppInfo {
        name: app
            .localizedName()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "(unnamed)".to_string()),
        bundle_id: app.bundleIdentifier().map(|b| b.to_string()),
        pid: app.processIdentifier(),
        active: app.isActive(),
        regular: app.activationPolicy() == NSApplicationActivationPolicy::Regular,
    }
}

fn sort_apps(apps: &mut [AppInfo]) {
    apps.sort_by_key(|a| (!a.active, !a.regular, a.name.to_lowercase()));
}

/// [`list_apps`], corrected for apps that started or exited since this process
/// first asked.
///
/// `NSWorkspace.runningApplications` is maintained by notifications delivered
/// on the *main* run loop, and cua-rs never runs one — its main thread is the
/// tokio runtime and its AX work lives on `cua-native`. Measured on macOS 26:
/// a Calculator launched after the first read stayed absent from the list on
/// every thread until the main run loop was spun, while
/// `runningApplicationWithProcessIdentifier` found it immediately. So an app
/// launched after the server started — by `launch_app`, or by the human — was
/// unresolvable by name for the life of the server.
///
/// The fix keeps the cached list's semantics and adds what it misses: every
/// pid from `proc_listallpids` that the cache does not know is looked up
/// directly, and cached entries whose process has exited are dropped. A full
/// pass measured 15–35 ms over ~700 pids, so [`resolve_app`] pays it only when
/// the cached list matched nothing, and `launch_app`/`close_app` pay it where
/// freshness is the whole question.
pub fn list_apps_fresh() -> Vec<AppInfo> {
    let mut apps: Vec<AppInfo> = list_apps()
        .into_iter()
        .filter(|a| pid_alive(a.pid))
        .collect();
    for pid in all_pids() {
        if pid <= 0 || apps.iter().any(|a| a.pid == pid) {
            continue;
        }
        if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
            apps.push(app_info(&app));
        }
    }
    sort_apps(&mut apps);
    apps
}

/// Every pid on the system, via `proc_listallpids`. Empty on failure.
fn all_pids() -> Vec<libc::pid_t> {
    // Ask for the count first, then leave headroom for processes started in
    // between; a short read is retried once with a buffer twice the size.
    // SAFETY: a null buffer with size 0 asks only for the count.
    let estimate = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let mut capacity = usize::try_from(estimate).unwrap_or(0) + 256;
    for _ in 0..2 {
        let mut pids = vec![0 as libc::pid_t; capacity];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: `pids` is a live buffer of exactly `bytes` bytes.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        let Ok(n) = usize::try_from(n) else {
            return Vec::new();
        };
        if n < pids.len() {
            pids.truncate(n);
            return pids;
        }
        capacity *= 2;
    }
    Vec::new()
}

/// Whether a pid names a live process (including one this user cannot signal).
pub fn pid_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs the existence and permission check only.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// One running app by pid, read directly rather than from the cached list.
pub fn app_for_pid(pid: libc::pid_t) -> Option<AppInfo> {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid).map(|a| app_info(&a))
}

/// Why an app string could not be turned into exactly one pid.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ResolveError {
    #[error("no running app matches `{query}`. Call list_apps to see what is running")]
    NotFound { query: String },

    /// Deliberately an error rather than a guess. Silently picking one of two
    /// matching apps is how an agent ends up typing a message into the wrong
    /// window, which is not recoverable by retrying.
    #[error("`{query}` is ambiguous: it matches {matches}. Use the bundle identifier instead")]
    Ambiguous { query: String, matches: String },
}

/// Resolve an app name, bundle identifier, or bundle path to one running app.
///
/// Accepts what a model will actually produce. Matching runs from most to least
/// specific, and only falls through to fuzzier rules when the precise ones find
/// nothing, so `Notes` can never be beaten by a substring hit on
/// `Notes Widget Extension`:
///
/// 1. exact bundle identifier — `com.apple.Notes`
/// 2. exact name, case-insensitive — `notes`
/// 3. bundle path basename — `/Applications/Notes.app`
/// 4. bundle-identifier suffix — `Notes` matching `com.apple.Notes`
/// 5. name prefix, then name substring
pub fn resolve_app(query: &str) -> Result<AppInfo, ResolveError> {
    match resolve_in(query, &list_apps()) {
        // The cached list can be missing an app started after this process
        // first asked; see [`list_apps_fresh`]. Only a miss pays for the scan.
        Err(ResolveError::NotFound { .. }) => resolve_in(query, &list_apps_fresh()),
        other => other,
    }
}

fn resolve_in(query: &str, apps: &[AppInfo]) -> Result<AppInfo, ResolveError> {
    let q = query.trim();

    // A bundle path is the one form that needs normalizing before comparison.
    let basename = q
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(q)
        .trim_end_matches(".app");

    let lower = q.to_lowercase();
    let base_lower = basename.to_lowercase();

    // Ordered candidate predicates. `regular` apps win inside every tier so a
    // helper process never shadows the real app the user meant.
    let tiers: [&dyn Fn(&AppInfo) -> bool; 6] = [
        &|a: &AppInfo| a.bundle_id.as_deref() == Some(q),
        &|a: &AppInfo| a.name.to_lowercase() == lower,
        &|a: &AppInfo| a.name.to_lowercase() == base_lower,
        &|a: &AppInfo| {
            a.bundle_id
                .as_deref()
                .is_some_and(|b| b.to_lowercase().ends_with(&format!(".{base_lower}")))
        },
        &|a: &AppInfo| a.name.to_lowercase().starts_with(&base_lower),
        &|a: &AppInfo| a.name.to_lowercase().contains(&base_lower),
    ];

    for mut matches in tiers.iter().map(|p| filter_preferring_regular(apps, *p)) {
        match matches.len() {
            0 => continue,
            1 => {
                if let Some(only) = matches.pop() {
                    return Ok(only);
                }
            }
            _ => {
                return Err(ResolveError::Ambiguous {
                    query: q.to_string(),
                    matches: describe(&matches),
                })
            }
        }
    }

    Err(ResolveError::NotFound {
        query: q.to_string(),
    })
}

/// Apply `pred`, then drop background helpers *if* any real app matched.
///
/// This is what stops "Slack" from being ambiguous between Slack and its four
/// helper processes, while still allowing a menu-bar-only app to be driven when
/// it is the only thing that matches.
fn filter_preferring_regular(apps: &[AppInfo], pred: impl Fn(&AppInfo) -> bool) -> Vec<AppInfo> {
    let hits: Vec<AppInfo> = apps.iter().filter(|a| pred(a)).cloned().collect();
    if hits.iter().any(|a| a.regular) {
        hits.into_iter().filter(|a| a.regular).collect()
    } else {
        hits
    }
}

fn describe(apps: &[AppInfo]) -> String {
    apps.iter()
        .map(|a| match &a.bundle_id {
            Some(b) => format!("{} ({b})", a.name),
            None => format!("{} (pid {})", a.name, a.pid),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The pid the workspace currently considers frontmost.
///
/// `NSWorkspace.frontmostApplication` rather than scanning [`list_apps`] for
/// `active`: `NSRunningApplication.isActive` was measured to lag behind an
/// activation by more than a second, long enough for a poll to conclude the
/// activation failed and refuse — while the app came forward anyway. This is
/// the answer the window server actually acts on.
pub fn frontmost_pid() -> Option<libc::pid_t> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|a| a.processIdentifier())
}

/// Bring an app to the front, the way clicking its Dock icon would.
///
/// This is `NSRunningApplication.activate`, not an event: no cursor moves and
/// nothing is typed. It still changes what the human is looking at — and if
/// the app's windows live on another Space, macOS switches Spaces — so it is
/// never called by the MCP action paths. It remains available for the explicit
/// `activate_probe` diagnostic.
///
/// Returns whether AppKit accepted the request. Activation is asynchronous
/// either way, so callers must poll for the state they actually need rather
/// than trusting `true`.
pub fn activate(pid: libc::pid_t) -> bool {
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return false;
    };
    // `ActivateAllWindows` rather than the default: a click aimed at a
    // background window of an app whose *other* window is frontmost would
    // otherwise stay occluded by its own sibling.
    app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows)
}

// ── launching and quitting ──────────────────────────────────────────────────

/// How a `launch_app` argument names an app that is not running yet.
///
/// Classified before anything touches the file system so the rule is testable
/// and so an error can say which reading was used. A running app is found by
/// [`resolve_app`]'s fuzzy tiers; an app that is not running has no process to
/// match against, so this is deliberately stricter — three exact forms and no
/// substring matching, because "launch whichever app contains `note`" is not a
/// request anyone makes on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSpec {
    /// `/Applications/TextEdit.app`, or anything containing a `/`.
    Path(String),
    /// `com.apple.TextEdit`: dotted, no spaces, not ending in `.app`.
    BundleId(String),
    /// `TextEdit`: looked up as `<name>.app` in the standard Applications
    /// folders only.
    Name(String),
}

impl LaunchSpec {
    pub fn parse(spec: &str) -> Option<Self> {
        let s = spec.trim();
        if s.is_empty() {
            return None;
        }
        if s.contains('/') {
            return Some(Self::Path(s.to_string()));
        }
        if s.to_lowercase().ends_with(".app") {
            let stem = s.get(..s.len() - 4).unwrap_or(s);
            return Some(Self::Name(stem.to_string()));
        }
        if s.contains('.') && !s.contains(char::is_whitespace) {
            return Some(Self::BundleId(s.to_string()));
        }
        Some(Self::Name(s.to_string()))
    }
}

/// An installed app, located but not launched.
pub struct LaunchTarget {
    pub url: Retained<NSURL>,
    pub path: String,
    pub name: String,
    pub bundle_id: Option<String>,
}

/// Folders a bare app name is looked up in, in order.
fn application_folders() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![
        std::path::PathBuf::from("/Applications"),
        std::path::PathBuf::from("/System/Applications"),
        std::path::PathBuf::from("/System/Applications/Utilities"),
        std::path::PathBuf::from("/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join("Applications"));
    }
    dirs
}

/// Turn a `launch_app` argument into one installed app bundle, or say why not.
pub fn locate_app(spec: &str) -> Result<LaunchTarget, String> {
    let parsed = LaunchSpec::parse(spec)
        .ok_or_else(|| "pass a bundle identifier, an app name, or a path to a .app".to_string())?;
    let path = match &parsed {
        LaunchSpec::Path(p) => {
            let path = std::path::Path::new(p);
            if !path.is_dir() {
                return Err(format!("{p} is not an app bundle on this machine"));
            }
            p.trim_end_matches('/').to_string()
        }
        LaunchSpec::BundleId(id) => {
            let url = NSWorkspace::sharedWorkspace()
                .URLForApplicationWithBundleIdentifier(&NSString::from_str(id))
                .ok_or_else(|| {
                    format!(
                        "no installed app has the bundle identifier `{id}`. Bundle identifiers are \
                         exact and case-sensitive; an app name or a path to the .app also works"
                    )
                })?;
            url.path()
                .map(|p| p.to_string())
                .ok_or_else(|| format!("LaunchServices located `{id}` at a URL with no path"))?
        }
        LaunchSpec::Name(name) => application_folders()
            .into_iter()
            .map(|dir| dir.join(format!("{name}.app")))
            .find(|p| p.is_dir())
            .map(|p| p.to_string_lossy().into_owned())
            .ok_or_else(|| {
                format!(
                    "no `{name}.app` in /Applications, /System/Applications or ~/Applications. Pass \
                     its bundle identifier or the full path to the .app instead"
                )
            })?,
    };

    let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
    let bundle_id = NSBundle::bundleWithURL(&url)
        .and_then(|b| b.bundleIdentifier())
        .map(|b| b.to_string());
    let name = path
        .rsplit('/')
        .next()
        .unwrap_or(&path)
        .trim_end_matches(".app")
        .to_string();
    Ok(LaunchTarget {
        url,
        path,
        name,
        bundle_id,
    })
}

/// How long to wait for LaunchServices to report a launch.
///
/// Generous: a cold launch of a large app is seconds, and the completion
/// handler fires once the process exists, not once it has drawn anything.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Launch an app through `NSWorkspace`, returning its pid.
///
/// `openApplicationAtURL:configuration:completionHandler:`, the public
/// replacement for the deprecated synchronous launch calls. Two configuration
/// choices are not defaults and both are deliberate:
///
/// - `activates` is the caller's, and `false` unless asked. A launch that comes
///   forward takes keyboard focus from whatever the human is typing into,
///   which is the one side effect this project exists to avoid (DESIGN.md §6).
/// - `promptsUserIfNeeded` is `false`. A quarantined or damaged app otherwise
///   gets a system dialog in front of the human, asking a question the agent
///   cannot see. Failing with LaunchServices' error is the honest outcome.
pub fn launch(target: &LaunchTarget, activate: bool) -> Result<libc::pid_t, String> {
    let config = NSWorkspaceOpenConfiguration::configuration();
    config.setActivates(activate);
    config.setPromptsUserIfNeeded(false);
    config.setAddsToRecentItems(false);

    let (tx, rx) = mpsc::channel::<Result<libc::pid_t, String>>();
    // The handler runs on a LaunchServices queue. Only plain data crosses back:
    // the pid, or the error text.
    let block = block2::RcBlock::new(move |app: *mut NSRunningApplication, error: *mut NSError| {
        let outcome = if !app.is_null() {
            // SAFETY: non-null, and valid for the duration of the handler.
            Ok(unsafe { &*app }.processIdentifier())
        } else if !error.is_null() {
            // SAFETY: as above.
            Err(unsafe { &*error }.localizedDescription().to_string())
        } else {
            Err("LaunchServices reported neither an app nor an error".to_string())
        };
        let _ = tx.send(outcome);
    });
    NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
        &target.url,
        &config,
        Some(&block),
    );
    match rx.recv_timeout(LAUNCH_TIMEOUT) {
        Ok(outcome) => outcome,
        Err(_) => Err(format!(
            "LaunchServices did not report back within {}s; the app may still be starting — \
             call list_apps to see whether it appeared",
            LAUNCH_TIMEOUT.as_secs()
        )),
    }
}

/// A running app with exactly this bundle identifier, preferring a regular one.
pub fn running_with_bundle_id(bundle_id: &str) -> Option<AppInfo> {
    let hits: Vec<AppInfo> = list_apps_fresh()
        .into_iter()
        .filter(|a| a.bundle_id.as_deref() == Some(bundle_id))
        .collect();
    hits.iter()
        .find(|a| a.regular)
        .or_else(|| hits.first())
        .cloned()
}

/// Ask an app to quit, the way its own Quit menu item would.
///
/// `NSRunningApplication.terminate` sends the quit Apple event, so the app runs
/// its normal shutdown: it may save state, and it may put up its own "save
/// changes?" sheet and wait. `forceTerminate` is never called here — a forced
/// quit discards unsaved work by definition, and no tool exposes it.
///
/// Returns whether the request was delivered, which is not whether the app
/// has quit.
pub fn request_quit(pid: libc::pid_t) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .is_some_and(|app| app.terminate())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, bundle: Option<&str>, regular: bool) -> AppInfo {
        AppInfo {
            name: name.to_string(),
            bundle_id: bundle.map(str::to_string),
            pid: 1,
            active: false,
            regular,
        }
    }

    #[test]
    fn regular_apps_beat_helper_processes_in_the_same_tier() {
        let apps = vec![
            app("Slack", Some("com.tinyspeck.slackmacgap"), true),
            app(
                "Slack Helper",
                Some("com.tinyspeck.slackmacgap.helper"),
                false,
            ),
            app(
                "Slack Helper (GPU)",
                Some("com.tinyspeck.slackmacgap.helper.GPU"),
                false,
            ),
        ];
        let hits = filter_preferring_regular(&apps, |a| a.name.to_lowercase().contains("slack"));
        assert_eq!(
            hits.len(),
            1,
            "helpers must not make a plain name ambiguous"
        );
        assert_eq!(hits[0].name, "Slack");
    }

    #[test]
    fn a_menu_bar_only_app_is_still_resolvable_when_alone() {
        let apps = vec![app("Rectangle", Some("com.knollsoft.Rectangle"), false)];
        let hits = filter_preferring_regular(&apps, |a| a.name == "Rectangle");
        assert_eq!(hits.len(), 1, "an agent-only app must stay reachable");
    }

    #[test]
    fn a_launch_spec_is_classified_by_its_shape() {
        assert_eq!(
            LaunchSpec::parse("com.apple.TextEdit"),
            Some(LaunchSpec::BundleId("com.apple.TextEdit".into()))
        );
        assert_eq!(
            LaunchSpec::parse("/Applications/Safari.app"),
            Some(LaunchSpec::Path("/Applications/Safari.app".into()))
        );
        assert_eq!(
            LaunchSpec::parse("TextEdit"),
            Some(LaunchSpec::Name("TextEdit".into()))
        );
        // A name with a version dot and a space is a name, not a bundle id.
        assert_eq!(
            LaunchSpec::parse("Visual Studio Code 1.9"),
            Some(LaunchSpec::Name("Visual Studio Code 1.9".into()))
        );
        assert_eq!(
            LaunchSpec::parse("TextEdit.app"),
            Some(LaunchSpec::Name("TextEdit".into()))
        );
        assert_eq!(LaunchSpec::parse("   "), None);
    }

    #[test]
    fn ambiguity_is_reported_not_guessed() {
        let apps = vec![
            app("Notes", Some("com.apple.Notes"), true),
            app("Notes Pro", Some("com.other.NotesPro"), true),
        ];
        // The substring tier matches both; that must surface as ambiguous
        // rather than silently choosing one.
        let hits = filter_preferring_regular(&apps, |a| a.name.to_lowercase().contains("notes"));
        assert_eq!(hits.len(), 2);
        assert!(describe(&hits).contains("com.apple.Notes"));
    }
}
