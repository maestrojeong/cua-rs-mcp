//! `Inner` methods for Notification Center. See `crate::notifications` for the
//! measured tree shape and DESIGN.md §13 for what was and was not verified.

use super::*;
use crate::notifications::{
    self as nc, DismissAllOutcome, Located, Notification, NotificationActed, NotificationKind,
    NotificationListing,
};

/// How long to wait for the history panel to appear, or a stack to expand.
const PANEL_WAIT_MS: u64 = 1_500;
/// How long to wait for a closed notification to leave the tree. Measured: gone
/// at the first re-read 800 ms after Close, on banners and history rows alike.
const GONE_WAIT_MS: u64 = 1_200;
const POLL_MS: u64 = 50;
/// Upper bound on `dismiss_all_notifications`, so a notification that keeps
/// reappearing cannot hold the worker forever.
const MAX_DISMISS: usize = 200;

/// Which action to press.
enum Press<'a> {
    Close,
    Named(&'a str),
}

fn wait_until(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if done() {
            return true;
        }
        if start.elapsed().as_millis() as u64 >= ms {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
}

/// The running app whose display name is exactly `name`, when there is one.
///
/// Exact (case-insensitive) on purpose: the fuzzy tiers `resolve_app` offers a
/// human typing an app name would let "Pass" resolve to Passwords, and this
/// answer feeds a safety gate. More than one match is no answer either.
fn resolve_source(name: &str) -> Option<AppInfo> {
    let want = name.trim().to_lowercase();
    let hits: Vec<AppInfo> = apps::list_apps()
        .into_iter()
        .filter(|a| a.name.to_lowercase() == want)
        .collect();
    let regular: Vec<&AppInfo> = hits.iter().filter(|a| a.regular).collect();
    match (regular.len(), hits.len()) {
        (1, _) => regular.first().map(|a| (*a).clone()),
        (0, 1) => hits.into_iter().next(),
        _ => None,
    }
}

fn short(n: &Notification) -> String {
    let what = [&n.title, &n.subtitle, &n.body]
        .iter()
        .filter_map(|p| p.as_deref())
        .collect::<Vec<_>>()
        .join(" / ");
    format!(
        "notification {} from `{}` ({what:?})",
        n.id,
        n.app.as_deref().unwrap_or("unknown app")
    )
}

impl Inner {
    fn notification_center(&self) -> Result<(AppInfo, Element)> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(nc::NOTIFICATION_CENTER_BUNDLE_ID).map_err(|e| {
            CoreError::NotificationCenterUnavailable {
                reason: format!("NotificationCenter could not be resolved ({e})"),
            }
        })?;
        let root = Element::for_pid(info.pid);
        // A wedged system process must not hold the single AX worker for the
        // default six seconds per attribute.
        let _ = root.set_timeout(2.0);
        Ok((info, root))
    }

    /// Press the menu-bar clock, which toggles the history panel. Gated as an
    /// action on ControlCenter, the process that owns the clock.
    fn toggle_history_panel(&mut self, verb: &'static str) -> Result<()> {
        // Resolved here rather than left to `acting`, which skips the gate for
        // an app it cannot resolve. That is right for an app the caller named
        // and wrong for one cua-rs picked itself.
        let cc = apps::resolve_app(nc::CONTROL_CENTER_BUNDLE_ID).map_err(|e| {
            CoreError::NotificationCenterUnavailable {
                reason: format!(
                    "ControlCenter, which owns the menu-bar clock, could not be resolved ({e})"
                ),
            }
        })?;
        let clock =
            nc::clock_extra(cc.pid).ok_or_else(|| CoreError::NotificationCenterUnavailable {
                reason:
                    "ControlCenter publishes no menu-bar clock (com.apple.menuextra.clock), so the \
                     history panel cannot be opened"
                        .into(),
            })?;
        let gate = crate::safety::Gate::labelled(
            verb,
            crate::safety::Candidate {
                role: "AXMenuBarItem".into(),
                label: Some("Clock".into()),
                description: "the menu-bar clock (toggles Notification Center)".into(),
                ..Default::default()
            },
        );
        let pid = cc.pid;
        self.acting(nc::CONTROL_CENTER_BUNDLE_ID, gate, false, move |i| {
            let before = i.watch(pid);
            clock.perform(cua_ax::action::PRESS)?;
            let changed = i.changed_since(pid, before);
            Ok(ActionResult::ax_at(
                cua_ax::action::PRESS,
                "the menu-bar clock".into(),
                changed,
                None,
            ))
        })?;
        Ok(())
    }

    /// Expand every collapsed stack, so each notification has its own row.
    /// `AXPress` on a stack in the history panel was measured to expand it.
    fn expand_stacks(
        &mut self,
        root: &Element,
        verb: &'static str,
        warnings: &mut Vec<String>,
    ) -> Result<()> {
        let stacks: Vec<Located> = nc::collect(root)
            .into_iter()
            .filter(|l| l.notification.kind == NotificationKind::CollapsedStack)
            .take(50)
            .collect();
        for stack in stacks {
            let id = stack.notification.id.clone();
            let gate = crate::safety::Gate::labelled(
                verb,
                crate::safety::Candidate {
                    role: "AXGroup".into(),
                    label: Some("Expand".into()),
                    description: format!(
                        "the collapsed stack topped by {}",
                        short(&stack.notification)
                    ),
                    ..Default::default()
                },
            );
            let el = stack.element.clone();
            self.acting(nc::NOTIFICATION_CENTER_BUNDLE_ID, gate, false, move |i| {
                let pid = el.pid()?;
                let before = i.watch(pid);
                el.perform(cua_ax::action::PRESS)?;
                let changed = i.changed_since(pid, before);
                Ok(ActionResult::ax_at(
                    cua_ax::action::PRESS,
                    "stack".into(),
                    changed,
                    None,
                ))
            })?;
            let expanded = wait_until(PANEL_WAIT_MS, || {
                !nc::collect(root).iter().any(|l| {
                    l.notification.id == id
                        && l.notification.kind == NotificationKind::CollapsedStack
                })
            });
            if !expanded {
                warnings.push(format!(
                    "the stack topped by {id} did not expand within {PANEL_WAIT_MS} ms; only its top notification is listed"
                ));
            }
        }
        Ok(())
    }

    /// Open the history panel if it is not open, and expand its stacks.
    /// Returns whether cua-rs opened it, so the caller can restore it.
    fn open_history(
        &mut self,
        root: &Element,
        verb: &'static str,
        warnings: &mut Vec<String>,
    ) -> Result<bool> {
        let mut opened = false;
        if !nc::history_open(root) {
            self.toggle_history_panel(verb)?;
            opened = true;
            if !wait_until(PANEL_WAIT_MS, || nc::history_open(root)) {
                // Measured: an empty history panel draws only "No Recent
                // Notifications" and has no list container, so from the tree
                // alone "empty" and "did not open" look the same. Say both.
                warnings.push(format!(
                    "no notification list appeared within {PANEL_WAIT_MS} ms of pressing the clock: the \
                     history is empty, or the panel did not open. Only notifications already on screen \
                     were read"
                ));
            }
        }
        self.expand_stacks(root, verb, warnings)?;
        Ok(opened)
    }

    /// Put the panel back the way it was. Best effort: failing to close it is a
    /// warning, not a reason to discard what the call already did.
    fn restore_history(&mut self, opened: bool, verb: &'static str, warnings: &mut Vec<String>) {
        if opened {
            if let Err(e) = self.toggle_history_panel(verb) {
                warnings.push(format!(
                    "cua-rs opened the history panel and could not close it again: {e}"
                ));
            }
        }
    }

    pub(super) fn list_notifications(
        &mut self,
        include_history: bool,
    ) -> Result<NotificationListing> {
        let (_, root) = self.notification_center()?;
        let mut warnings = Vec::new();
        // Without history this is a pure read and is not gated, like every
        // other read. With it, cua-rs presses the clock and expands stacks,
        // which are actions, and they pass through the gate as such.
        let opened = if include_history {
            self.open_history(&root, "list_notifications", &mut warnings)?
        } else {
            false
        };
        let notifications = nc::collect(&root)
            .into_iter()
            .map(|l| l.notification)
            .collect();
        self.restore_history(opened, "list_notifications", &mut warnings);
        Ok(NotificationListing {
            notifications,
            opened_panel: opened,
            warnings,
        })
    }

    /// Find `id` among what is on screen, then in history if allowed.
    fn locate(
        &mut self,
        root: &Element,
        id: &str,
        include_history: bool,
        verb: &'static str,
        warnings: &mut Vec<String>,
    ) -> Result<(Option<Located>, bool)> {
        let id = id.trim();
        let find = |root: &Element| {
            nc::collect(root)
                .into_iter()
                .find(|l| l.notification.id.eq_ignore_ascii_case(id))
        };
        let mut opened = false;
        let mut found = find(root);
        if found.is_none() && include_history {
            opened = self.open_history(root, verb, warnings)?;
            found = find(root);
        }
        // A stack shares its top notification's id. Pressing Close-or-anything
        // on the stack would act on every notification in it, so expand first
        // and act on the single row.
        if found
            .as_ref()
            .is_some_and(|l| l.notification.kind == NotificationKind::CollapsedStack)
        {
            self.expand_stacks(root, verb, warnings)?;
            found = find(root).filter(|l| l.notification.kind == NotificationKind::Single);
        }
        Ok((found, opened))
    }

    fn press_on(
        &mut self,
        nc_info: &AppInfo,
        root: &Element,
        loc: &Located,
        which: Press<'_>,
        verb: &'static str,
        confirm_destructive: bool,
    ) -> Result<NotificationActed> {
        let n = &loc.notification;
        let advertised = match which {
            Press::Close => loc.close_action(),
            Press::Named(label) => {
                // Only custom actions. `AXPress` is the banner's default click
                // and was deliberately not exercised on real hardware (it
                // opens the posting app), so it is not offered as verified.
                if label.trim().eq_ignore_ascii_case(cua_ax::action::PRESS) {
                    return Err(CoreError::NotificationActionUnavailable {
                        id: n.id.clone(),
                        action: label.to_string(),
                        available: n.actions.iter().filter(|a| *a != cua_ax::action::PRESS).cloned().collect::<Vec<_>>().join(", "),
                        why: "AXPress is the notification's default click, which hands off to the posting app; \
                              cua-rs exposes only the named custom actions Notification Center advertises".into(),
                    });
                }
                loc.advertised(label)
            }
        };
        let Some(advertised) = advertised else {
            return Err(CoreError::NotificationActionUnavailable {
                id: n.id.clone(),
                action: match which {
                    Press::Close => "Close".to_string(),
                    Press::Named(l) => l.to_string(),
                },
                available: n.actions.iter().filter(|a| *a != cua_ax::action::PRESS).cloned().collect::<Vec<_>>().join(", "),
                why: match which {
                    Press::Close => "none of its actions is a Close cua-rs recognizes. Close was measured as `닫기` \
                                     on a Korean system; other locales use an expected translation that has not \
                                     been verified — pass the label shown here to notification_action instead"
                        .into(),
                    Press::Named(_) => "it does not advertise an action with that label. App-defined buttons \
                                        (Reply, Snooze…) have not been verified to appear here on macOS 26.2"
                        .into(),
                },
            });
        };
        let label = nc::action_label(&advertised).to_string();
        let source = n.app.clone();
        let source_bundle = source
            .as_deref()
            .and_then(resolve_source)
            .and_then(|a| a.bundle_id);
        let described = short(n);
        let gate = crate::safety::Gate::labelled(
            verb,
            crate::safety::Candidate {
                role: "AXButton".into(),
                label: Some(label.clone()),
                description: described.clone(),
                ..Default::default()
            },
        )
        .confirmed(confirm_destructive);
        let el = loc.element.clone();
        let pid = nc_info.pid;
        let verb_label = format!("{label} (Notification Center custom action)");
        let result = self.acting(nc::NOTIFICATION_CENTER_BUNDLE_ID, gate, false, move |i| {
            // After the ordinary gates on NotificationCenter, the one on the
            // app the notification is about. See `safety::notification_source`.
            crate::safety::guard_notification_source(
                verb,
                source.as_deref(),
                source_bundle.as_deref(),
            )?;
            let before = i.watch(pid);
            el.perform(&advertised)?;
            let changed = i.changed_since(pid, before);
            Ok(ActionResult::ax_at(verb_label, described, changed, None))
        })?;
        let id = n.id.clone();
        // Close is expected to remove it, so wait for that; any other action is
        // only observed once, briefly, rather than stalling the call.
        let wait_ms = if matches!(which, Press::Close) {
            GONE_WAIT_MS
        } else {
            300
        };
        let gone = wait_until(wait_ms, || {
            !nc::collect(root).iter().any(|l| l.notification.id == id)
        });
        Ok(NotificationActed {
            result,
            notification: n.clone(),
            label,
            gone,
            warnings: Vec::new(),
        })
    }

    fn act_on_one(
        &mut self,
        id: &str,
        which: Press<'_>,
        include_history: bool,
        verb: &'static str,
        confirm_destructive: bool,
    ) -> Result<NotificationActed> {
        let (info, root) = self.notification_center()?;
        let mut warnings = Vec::new();
        let (found, opened) = self.locate(&root, id, include_history, verb, &mut warnings)?;
        let outcome = match found {
            Some(loc) => self.press_on(&info, &root, &loc, which, verb, confirm_destructive),
            None => Err(CoreError::NotificationNotFound {
                id: id.trim().to_string(),
                searched_history: include_history,
            }),
        };
        self.restore_history(opened, verb, &mut warnings);
        outcome.map(|mut acted| {
            acted.warnings.extend(warnings);
            acted
        })
    }

    pub(super) fn dismiss_notification(
        &mut self,
        id: &str,
        include_history: bool,
    ) -> Result<NotificationActed> {
        self.act_on_one(
            id,
            Press::Close,
            include_history,
            "dismiss_notification",
            false,
        )
    }

    pub(super) fn notification_action(
        &mut self,
        id: &str,
        action: &str,
        include_history: bool,
        confirm_destructive: bool,
    ) -> Result<NotificationActed> {
        self.act_on_one(
            id,
            Press::Named(action),
            include_history,
            "notification_action",
            confirm_destructive,
        )
    }

    pub(super) fn dismiss_all_notifications(
        &mut self,
        include_history: bool,
    ) -> Result<DismissAllOutcome> {
        const VERB: &str = "dismiss_all_notifications";
        let (info, root) = self.notification_center()?;
        let mut warnings = Vec::new();
        let opened = if include_history {
            self.open_history(&root, VERB, &mut warnings)?
        } else {
            self.expand_stacks(&root, VERB, &mut warnings)?;
            false
        };
        let mut dismissed = Vec::new();
        let mut stopped_on = None;
        let mut attempted: HashSet<String> = HashSet::new();
        for _ in 0..MAX_DISMISS {
            // Re-read every time: a removal reflows the list, and positions
            // were measured to shift by one after a single Close.
            let next = nc::collect(&root).into_iter().find(|l| {
                l.notification.kind == NotificationKind::Single
                    && !attempted.contains(&l.notification.id)
            });
            let Some(loc) = next else { break };
            attempted.insert(loc.notification.id.clone());
            match self.press_on(&info, &root, &loc, Press::Close, VERB, false) {
                Ok(acted) if acted.gone => dismissed.push(acted.notification),
                Ok(acted) => {
                    stopped_on = Some((
                        acted.notification,
                        format!("Close was delivered but the notification was still listed {GONE_WAIT_MS} ms later"),
                    ));
                    break;
                }
                Err(e) => {
                    stopped_on = Some((loc.notification.clone(), e.to_string()));
                    break;
                }
            }
        }
        let remaining = nc::collect(&root).len();
        self.restore_history(opened, VERB, &mut warnings);
        Ok(DismissAllOutcome {
            dismissed,
            stopped_on,
            remaining,
            warnings,
        })
    }
}
