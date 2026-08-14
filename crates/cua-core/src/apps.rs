//! Finding the app the agent asked for.

use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication,
    NSWorkspace,
};

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
        .map(|app| {
            let policy = app.activationPolicy();
            AppInfo {
                name: app
                    .localizedName()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "(unnamed)".to_string()),
                bundle_id: app.bundleIdentifier().map(|b| b.to_string()),
                pid: app.processIdentifier(),
                active: app.isActive(),
                regular: policy == NSApplicationActivationPolicy::Regular,
            }
        })
        // Processes with no GUI at all cannot be driven and only add noise.
        .filter(|a| a.pid > 0)
        .collect();

    out.sort_by_key(|a| (!a.active, !a.regular, a.name.to_lowercase()));
    out
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
    let q = query.trim();
    let apps = list_apps();

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

    for matches in tiers.iter().map(|p| filter_preferring_regular(&apps, *p)) {
        match matches.len() {
            0 => continue,
            1 => return Ok(matches.into_iter().next().unwrap()),
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
