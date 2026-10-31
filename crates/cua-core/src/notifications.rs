//! Notification Center: reading banners and history, and pressing their actions.
//!
//! Notifications are not drawn by the app that posted them. Banners and the
//! history panel both belong to `NotificationCenter.app`
//! (`com.apple.notificationcenterui`), and the history panel is opened from the
//! menu-bar clock, which belongs to a third process, `ControlCenter`. So every
//! operation here reads one system process and may press an element in another,
//! and the app a notification is *about* is known only by the localized display
//! name NotificationCenter writes into the group's `AXDescription`.
//!
//! Everything below was measured on macOS 26.2 (25C56) with
//! `examples/notification_probe.rs` rather than assumed; DESIGN.md §13 has the
//! numbers and the gaps. The shape this module relies on:
//!
//! - each notification is an `AXGroup` with subrole `AXNotificationCenterBanner`
//!   whose `AXIdentifier` is the notification's own UUID — the same UUID usernoted
//!   logs for the record, stable from banner to history;
//! - its title, subtitle and body are `AXStaticText` children identified `title`,
//!   `subtitle` and `body`; history rows add an unidentified time stamp;
//! - its controls are *custom actions* (`Name:<label>\nTarget:..\nSelector:..`),
//!   localized, with Close among them — there is no separate close button;
//! - several notifications from one app collapse into an
//!   `AXNotificationCenterBannerStack` in the history panel, whose identifier is
//!   the top notification's UUID and whose `AXPress` expands it.
//!
//! Pure parsing is separated from the AX walk so it can be unit-tested; the walk
//! itself needs a live Notification Center and is exercised by the probe.

use cua_ax::Element;

/// NotificationCenter.app, which draws banners and the history panel.
pub const NOTIFICATION_CENTER_BUNDLE_ID: &str = "com.apple.notificationcenterui";
/// ControlCenter, which owns the menu-bar clock that opens the history panel.
pub const CONTROL_CENTER_BUNDLE_ID: &str = "com.apple.controlcenter";

const CLOCK_EXTRA_ID: &str = "com.apple.menuextra.clock";
const LIST_ITEMS_ID: &str = "AXNotificationListItems";
const SUBROLE_SINGLE: &str = "AXNotificationCenterBanner";
/// Not observed on 26.2 (only banner-style notifications were available to
/// post), but it is the subrole AppKit documents for alert-style ones, and
/// treating it as a notification costs nothing.
const SUBROLE_ALERT: &str = "AXNotificationCenterAlert";
const SUBROLE_STACK: &str = "AXNotificationCenterBannerStack";

/// Depth bound for the walk. The deepest notification measured sits at depth 6
/// (`window > group > group > scroll area > list > banner`); 12 leaves room for
/// a redesign without letting a pathological tree run away.
const MAX_DEPTH: usize = 12;
/// Node bound for the walk, for the same reason.
const MAX_NODES: usize = 4_000;

/// Close, as NotificationCenter labels it. Only 닫기 (ko) was measured; the
/// others are Apple's usual translations of the same control and are listed so
/// the tool degrades to "not found, here is what it offers" rather than to a
/// silent miss on an English system. See DESIGN.md §13.
const CLOSE_LABELS: &[&str] = &[
    "close",
    "닫기",
    "閉じる",
    "关闭",
    "關閉",
    "schließen",
    "fermer",
    "cerrar",
    "chiudi",
    "fechar",
    "sluiten",
];

/// Whether a notification stands alone or is the visible top of a collapsed
/// stack of several from the same app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum NotificationKind {
    Single,
    CollapsedStack,
}

/// One notification as cua-rs reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Notification {
    /// NotificationCenter's own UUID for the record. Stable across re-reads and
    /// from banner to history, so unlike a tree position it does not go stale
    /// when a neighbour is removed. Only goes stale when the notification does.
    pub id: String,
    /// The posting app's localized display name, recovered from the group's
    /// description. `None` when the description did not have the measured shape.
    pub app: Option<String>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub body: Option<String>,
    /// The time stamp history rows draw, verbatim (`오전 1:03`). Banners have none.
    pub time: Option<String>,
    pub kind: NotificationKind,
    /// Labels of the custom actions it advertises, in order, e.g.
    /// `["Show Details", "Show", "Close"]`.
    pub actions: Vec<String>,
    /// Whether it was read from the history panel rather than a live banner.
    pub in_history: bool,
}

/// The raw attributes the walk reads off one notification group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawNotification {
    pub subrole: String,
    pub identifier: Option<String>,
    pub description: Option<String>,
    /// `(AXIdentifier, AXValue)` of each static-text child.
    pub texts: Vec<(Option<String>, String)>,
    /// Advertised action names, verbatim.
    pub actions: Vec<String>,
}

/// The label of an advertised action.
///
/// Custom actions arrive as `Name:<label>\nTarget:0x0\nSelector:(null)`; a
/// standard action (`AXPress`) is its own label.
pub fn action_label(advertised: &str) -> &str {
    match advertised.strip_prefix("Name:") {
        Some(rest) => rest.split('\n').next().unwrap_or(rest).trim(),
        None => advertised,
    }
}

/// Whether `label` is NotificationCenter's Close control in a known locale.
pub fn is_close_label(label: &str) -> bool {
    let norm = label.trim().to_lowercase();
    CLOSE_LABELS.contains(&norm.as_str())
}

/// Whether an AX subrole marks a notification group.
pub fn is_notification_subrole(subrole: &str) -> bool {
    matches!(subrole, SUBROLE_SINGLE | SUBROLE_ALERT | SUBROLE_STACK)
}

/// Turn one group's raw attributes into a [`Notification`].
///
/// `None` when it is not a notification or has no identifier: without the UUID
/// there is nothing stable to hand back, and an index would go stale on the
/// next removal (measured: one Close shifted every later row up by one).
pub fn parse_notification(raw: &RawNotification, in_history: bool) -> Option<Notification> {
    if !is_notification_subrole(&raw.subrole) {
        return None;
    }
    let id = raw
        .identifier
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let text = |want: &str| {
        raw.texts
            .iter()
            .find(|(tid, _)| tid.as_deref() == Some(want))
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let title = text("title");
    let subtitle = text("subtitle");
    let body = text("body");
    let time = raw
        .texts
        .iter()
        .find(|(tid, v)| tid.is_none() && !v.is_empty())
        .map(|(_, v)| v.clone());
    let app = raw
        .description
        .as_deref()
        .and_then(|d| source_app_name(d, [&title, &subtitle, &body]));
    Some(Notification {
        id: id.to_string(),
        app,
        title,
        subtitle,
        body,
        time,
        kind: if raw.subrole == SUBROLE_STACK {
            NotificationKind::CollapsedStack
        } else {
            NotificationKind::Single
        },
        actions: raw
            .actions
            .iter()
            .map(|a| action_label(a).to_string())
            .collect(),
        in_history,
    })
}

/// Recover the posting app's display name from a group description.
///
/// Measured shapes: a single notification reads `"<app> <title>, <subtitle>,
/// <body>"`, a collapsed stack `"<app>, <title>, <body>, <stack word>"`. In both
/// the app name is whatever precedes the joined texts, minus the separator.
/// Requires the joined texts to be present verbatim; anything else returns
/// `None` rather than a guess, because this name feeds a safety gate.
fn source_app_name(description: &str, parts: [&Option<String>; 3]) -> Option<String> {
    let joined: Vec<&str> = parts.iter().filter_map(|p| p.as_deref()).collect();
    if joined.is_empty() {
        return None;
    }
    let joined = joined.join(", ");
    // Single: exactly "<app> <joined>".
    let single = description
        .strip_suffix(joined.as_str())
        .and_then(|p| p.strip_suffix(' '));
    // Stack: "<app>, <joined>, <stack word>" (or with nothing after).
    let stacked = || {
        let marker = format!(", {joined}");
        let at = description.find(&marker)?;
        let after = &description[at + marker.len()..];
        (after.is_empty() || after.starts_with(", ")).then(|| &description[..at])
    };
    let prefix = single.or_else(stacked)?.trim();
    (!prefix.is_empty()).then(|| prefix.to_string())
}

// ── live reads ───────────────────────────────────────────────────────────────

/// A notification together with the element that carries its actions.
pub(crate) struct Located {
    pub element: Element,
    pub notification: Notification,
}

impl Located {
    /// The full advertised name of the action labelled `label`, if offered.
    pub fn advertised(&self, label: &str) -> Option<String> {
        let want = label.trim().to_lowercase();
        self.element
            .actions()
            .into_iter()
            .find(|a| action_label(a).to_lowercase() == want)
    }

    /// The advertised Close action, if this notification offers one.
    pub fn close_action(&self) -> Option<String> {
        self.element
            .actions()
            .into_iter()
            .find(|a| is_close_label(action_label(a)))
    }
}

/// Every notification currently in NotificationCenter's tree.
pub(crate) fn collect(root: &Element) -> Vec<Located> {
    let mut out = Vec::new();
    let mut budget = MAX_NODES;
    for child in root.children() {
        walk(&child, 1, false, &mut budget, &mut out);
    }
    out
}

fn walk(e: &Element, depth: usize, in_history: bool, budget: &mut usize, out: &mut Vec<Located>) {
    if *budget == 0 || depth > MAX_DEPTH {
        return;
    }
    *budget -= 1;
    // NotificationCenter's own app menu (Apple / File / Edit) is a large tree
    // with nothing to do with notifications.
    if e.role().as_deref() == Some("AXMenuBar") {
        return;
    }
    let in_history = in_history || e.string("AXIdentifier").as_deref() == Some(LIST_ITEMS_ID);
    if let Some(subrole) = e.string("AXSubrole").filter(|s| is_notification_subrole(s)) {
        let raw = RawNotification {
            subrole,
            identifier: e.string("AXIdentifier"),
            description: e.string("AXDescription"),
            texts: e
                .children()
                .iter()
                .filter(|c| c.role().as_deref() == Some("AXStaticText"))
                .filter_map(|c| {
                    c.value_string("AXValue")
                        .map(|v| (c.string("AXIdentifier"), v))
                })
                .collect(),
            actions: e.actions(),
        };
        if let Some(notification) = parse_notification(&raw, in_history) {
            out.push(Located {
                element: e.clone(),
                notification,
            });
        }
        return;
    }
    for c in e.children() {
        walk(&c, depth + 1, in_history, budget, out);
    }
}

/// Whether the history panel is open: its list container is in the tree.
pub(crate) fn history_open(root: &Element) -> bool {
    fn find(e: &Element, depth: usize, budget: &mut usize) -> bool {
        if *budget == 0 || depth > MAX_DEPTH || e.role().as_deref() == Some("AXMenuBar") {
            return false;
        }
        *budget -= 1;
        if e.string("AXIdentifier").as_deref() == Some(LIST_ITEMS_ID) {
            return true;
        }
        e.children().iter().any(|c| find(c, depth + 1, budget))
    }
    let mut budget = MAX_NODES;
    root.children().iter().any(|c| find(c, 1, &mut budget))
}

/// The menu-bar clock, which toggles the history panel when pressed.
pub(crate) fn clock_extra(control_center_pid: libc::pid_t) -> Option<Element> {
    Element::for_pid(control_center_pid)
        .element("AXExtrasMenuBar")?
        .children()
        .into_iter()
        .find(|i| i.string("AXIdentifier").as_deref() == Some(CLOCK_EXTRA_ID))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(subrole: &str, id: &str, desc: &str, texts: &[(Option<&str>, &str)]) -> RawNotification {
        RawNotification {
            subrole: subrole.into(),
            identifier: Some(id.into()),
            description: Some(desc.into()),
            texts: texts
                .iter()
                .map(|(i, v)| (i.map(str::to_string), v.to_string()))
                .collect(),
            actions: vec![
                "AXPress".into(),
                "Name:세부사항 보기\nTarget:0x0\nSelector:(null)".into(),
                "Name:보기\nTarget:0x0\nSelector:(null)".into(),
                "Name:닫기\nTarget:0x0\nSelector:(null)".into(),
            ],
        }
    }

    #[test]
    fn custom_action_labels_are_unwrapped() {
        assert_eq!(
            action_label("Name:닫기\nTarget:0x0\nSelector:(null)"),
            "닫기"
        );
        assert_eq!(action_label("Name:Reply\nTarget:0x1"), "Reply");
        assert_eq!(action_label("AXPress"), "AXPress");
        assert_eq!(action_label("Name:Close"), "Close");
    }

    #[test]
    fn close_matches_known_locales_only() {
        assert!(is_close_label("닫기"));
        assert!(is_close_label(" Close "));
        assert!(is_close_label("CLOSE"));
        assert!(!is_close_label("Close Account"));
        assert!(!is_close_label("모두 지우기"));
        assert!(!is_close_label("보기"));
    }

    // Shapes copied verbatim from the probe's output on macOS 26.2.
    #[test]
    fn single_banner_with_subtitle() {
        let n = parse_notification(
            &raw(
                "AXNotificationCenterBanner",
                "805A1087-89B8-482B-B822-30F2726D4663",
                "스크립트 편집기 probe title, probe sub, probe body one",
                &[
                    (Some("title"), "probe title"),
                    (Some("subtitle"), "probe sub"),
                    (Some("body"), "probe body one"),
                ],
            ),
            false,
        )
        .unwrap();
        assert_eq!(n.id, "805A1087-89B8-482B-B822-30F2726D4663");
        assert_eq!(n.app.as_deref(), Some("스크립트 편집기"));
        assert_eq!(n.title.as_deref(), Some("probe title"));
        assert_eq!(n.subtitle.as_deref(), Some("probe sub"));
        assert_eq!(n.body.as_deref(), Some("probe body one"));
        assert_eq!(n.time, None);
        assert_eq!(n.kind, NotificationKind::Single);
        assert_eq!(n.actions, vec!["AXPress", "세부사항 보기", "보기", "닫기"]);
        assert!(!n.in_history);
    }

    #[test]
    fn history_row_keeps_its_time_stamp() {
        let n = parse_notification(
            &raw(
                "AXNotificationCenterBanner",
                "9F7D44F4-1F03-4C4C-A9AA-587FE4F4FF71",
                "스크립트 편집기 probe2, b2",
                &[
                    (Some("title"), "probe2"),
                    (Some("body"), "b2"),
                    (None, "오전 1:03"),
                ],
            ),
            true,
        )
        .unwrap();
        assert_eq!(n.app.as_deref(), Some("스크립트 편집기"));
        assert_eq!(n.time.as_deref(), Some("오전 1:03"));
        assert!(n.in_history);
    }

    #[test]
    fn collapsed_stack_is_reported_as_such() {
        let n = parse_notification(
            &raw(
                "AXNotificationCenterBannerStack",
                "6B5207BF-3B2F-4025-B7FF-46665C40260F",
                "스크립트 편집기, hist 3, hist body 3, 스택",
                &[(Some("title"), "hist 3"), (Some("body"), "hist body 3")],
            ),
            true,
        )
        .unwrap();
        assert_eq!(n.kind, NotificationKind::CollapsedStack);
        assert_eq!(n.app.as_deref(), Some("스크립트 편집기"));
    }

    #[test]
    fn no_identifier_means_no_notification() {
        let mut r = raw(
            "AXNotificationCenterBanner",
            "x",
            "App t",
            &[(Some("title"), "t")],
        );
        r.identifier = None;
        assert!(parse_notification(&r, false).is_none());
        r.identifier = Some("   ".into());
        assert!(parse_notification(&r, false).is_none());
    }

    #[test]
    fn non_notification_groups_are_ignored() {
        let r = raw("AXHostingView", "id", "날씨", &[]);
        assert!(parse_notification(&r, false).is_none());
    }

    #[test]
    fn unrecognized_description_yields_no_app_rather_than_a_guess() {
        let n = parse_notification(
            &raw(
                "AXNotificationCenterBanner",
                "id",
                "something else entirely",
                &[(Some("title"), "t")],
            ),
            false,
        )
        .unwrap();
        assert_eq!(n.app, None);
        let n = parse_notification(
            &raw(
                "AXNotificationCenterBanner",
                "id",
                "t",
                &[(Some("title"), "t")],
            ),
            false,
        )
        .unwrap();
        assert_eq!(n.app, None, "an empty prefix is not an app name");
    }
}

// ── results ──────────────────────────────────────────────────────────────────

/// What `list_notifications` read.
#[derive(Debug, Clone)]
pub struct NotificationListing {
    pub notifications: Vec<Notification>,
    /// Whether cua-rs opened the history panel to read it (and closed it again).
    pub opened_panel: bool,
    /// Anything that went sideways without failing the read.
    pub warnings: Vec<String>,
}

/// One action pressed on one notification.
#[derive(Debug, Clone)]
pub struct NotificationActed {
    pub result: crate::session::ActionResult,
    pub notification: Notification,
    /// The full label pressed.
    pub label: String,
    /// Whether the notification had left Notification Center's tree by the time
    /// cua-rs looked again. For Close this is the verification; for other
    /// actions it is just an observation.
    pub gone: bool,
    pub warnings: Vec<String>,
}

/// What `dismiss_all_notifications` managed before it stopped.
#[derive(Debug, Clone)]
pub struct DismissAllOutcome {
    pub dismissed: Vec<Notification>,
    /// The notification it stopped on, and why. `None` when it ran out of
    /// notifications rather than failing.
    pub stopped_on: Option<(Notification, String)>,
    /// Notifications still in the tree afterwards.
    pub remaining: usize,
    pub warnings: Vec<String>,
}
