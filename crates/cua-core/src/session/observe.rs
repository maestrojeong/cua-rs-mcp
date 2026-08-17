//! `Inner` methods for observe responsibilities.

use super::*;

impl Inner {
    pub(super) fn get_app_state(
        &mut self,
        query: &str,
        mut opts: StateOptions,
    ) -> Result<AppState> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;
        let app_el = Element::for_pid(info.pid);

        // Once per app, ask Chromium/Electron to build its tree, then let it
        // settle: the build is asynchronous, so reading immediately would return
        // the same empty window we are trying to fix.
        // `insert` returns false when the key was already present, which makes
        // "poke once per process lifetime" a single atomic step.
        let key = ProcessKey::for_pid(info.pid);
        let first_read = self.enabled.insert(key);
        if first_read {
            let enablement = app_el.enable_rich_accessibility();
            tracing::debug!(?enablement, pid = info.pid, "requested rich accessibility");
            self.enablement.insert(key, enablement);
            // A short settle, not a wait for completion. Some apps do publish
            // within a few hundred milliseconds; the ones that take seconds are
            // handled by telling the caller to ask again rather than by blocking
            // every first call for that long.
            std::thread::sleep(std::time::Duration::from_millis(400));
        }

        let mut warnings = Vec::new();

        // Reading a forbidden app stays allowed; photographing one does not.
        // The tree describes the UI holding a secret, while the pixels are the
        // secret. See `crate::safety` for the whole read/act split.
        if opts.include_screenshot {
            if let Some(why) = crate::safety::screenshot_refusal(&info) {
                opts.include_screenshot = false;
                warnings.push(why);
            }
        }

        // Prefer the focused window, fall back to main, then to the first one.
        // A minimized-only app has none of these, which is a real state and not
        // an error we can paper over.
        let window_el = app_el
            .element(cua_ax::attr::FOCUSED_WINDOW)
            .or_else(|| app_el.element(cua_ax::attr::MAIN_WINDOW))
            .or_else(|| app_el.elements(cua_ax::attr::WINDOWS).into_iter().next())
            .ok_or_else(|| CoreError::NoWindow {
                app: info.name.clone(),
            })?;

        // A scoped walk starts from an element the caller saw in the previous
        // snapshot, not from the window. Resolved before the new snapshot
        // replaces the old one, since that is where the index lives.
        let root = match opts.scope {
            None => window_el.clone(),
            Some(index) => {
                let snap = self
                    .snapshots
                    .get(&info.pid)
                    .ok_or_else(|| CoreError::NoSnapshot {
                        app: info.name.clone(),
                    })?;
                if snap.process_key != key {
                    return Err(CoreError::ProcessReplaced {
                        app: info.name.clone(),
                    });
                }
                let node = snap.nodes.get(index).ok_or(CoreError::BadIndex {
                    index,
                    count: snap.nodes.len(),
                })?;
                node.element.clone()
            }
        };

        let (nodes, complete) = root.snapshot_tree_reporting(opts.limits);

        // A small tree on the *first* read of an app is ambiguous, and the
        // ambiguity is worth stating rather than resolving badly.
        //
        // Chromium and Electron build their accessibility tree lazily once poked,
        // and it does not arrive promptly: Slack measured 13 elements for over
        // three seconds after the poke and 367 a minute later. Deciding "this
        // window is empty" from the first read is therefore wrong, and so is
        // deciding "this app refuses AX" — the read-back of
        // AXManualAccessibility is `false` on Slack even when it is plainly
        // working, so there is no signal there either.
        //
        // What is honest and actionable in both cases: say the tree may still be
        // building, say to ask again, and say what it means if it never grows.
        const LOOKS_EMPTY: usize = 20;
        if nodes.len() < LOOKS_EMPTY && first_read {
            warnings.push(format!(
                "only {} elements on the first read of this app. Chromium and Electron apps build \
                 their accessibility tree lazily after being asked, and it can take several \
                 seconds to appear — call get_app_state again. If it stays this small across a \
                 few tries, this app does not expose its web content over the accessibility API \
                 at all and has to be driven over CDP instead; its native chrome (window buttons, \
                 menu bar) is still reachable here.",
                nodes.len()
            ));
        }

        if nodes.len() >= opts.limits.max_nodes {
            warnings.push(format!(
                "tree truncated at {} elements; pass a larger max_nodes or narrow the target",
                opts.limits.max_nodes
            ));
        } else if !complete {
            // Truncation by *time*, which looks nothing like truncation by
            // count: the tree is short, so nothing suggests anything is
            // missing. Measured on KakaoTalk with ten windows open, a walk that
            // would have returned 2000 nodes took 171 s; the budget cuts that
            // to 10 s and 429 nodes, and the conversation the caller wanted was
            // in the part that never arrived. Without this line the caller
            // concludes the element does not exist.
            warnings.push(format!(
                "tree is INCOMPLETE: the walk hit its {:.0}s time budget after {} elements, so \
                 anything further down is missing rather than absent. This app is answering \
                 accessibility calls slowly. Narrow the walk with scope_element_id, or use find \
                 to search, before concluding an element is not there",
                opts.limits.budget.as_secs_f64(),
                nodes.len()
            ));
        }

        // Match the AX window to a ScreenCaptureKit window by pid + frame.
        // The direct route would be `_AXUIElementGetWindow`, which is a private
        // symbol; matching on public API keeps *window identity* off SPI and
        // thus off the "breaks on the next macOS release" risk. (Input
        // synthesis's quiet tier does use SkyLight SPI, but that lives in
        // cua-hid, not in this matching path.)
        let ax_frame = window_el.frame();
        // One enumeration, two answers. It costs p50 ~28 ms with a couple of
        // hundred windows live, so the pop-up list is mined from the same call
        // that identifies the window to capture rather than fetched again.
        let (window, popups) = match cua_capture::list_windows() {
            Ok(list) => (
                best_window_match(&list, info.pid, ax_frame),
                transient_popups(&list, info.pid, None),
            ),
            Err(e) => {
                // The tree is still useful without pixels, so this is a warning
                // and not a failure.
                warnings.push(e.to_string());
                (None, Vec::new())
            }
        };
        if !popups.is_empty() {
            // Said out loud because the tree below cannot say it. A pop-up is a
            // separate window with no accessibility representation, so a walk of
            // the target window is complete and still describes none of what the
            // user is actually looking at.
            warnings.push(format!(
                "this app has {} window(s) open above its content that the tree below does not \
                 describe and accessibility cannot see into — see the `transient UI` section. A \
                 menu item is reached with click_in_window against the pop-up's own window_id, or \
                 more cheaply with press_key if it shows a keyboard shortcut",
                popups.len()
            ));
        }

        let screenshot = match (opts.include_screenshot, &window) {
            (true, Some(w)) => match cua_capture::capture_window(w.id, opts.max_image_dim) {
                Ok(shot) => Some(Screenshot {
                    png: shot.png,
                    width: shot.width,
                    height: shot.height,
                    scale: shot.scale,
                    frame: shot.frame,
                    window_frame: shot.window_frame,
                }),
                Err(e) => {
                    warnings.push(capture_failure_warning(&e.to_string(), &nodes));
                    None
                }
            },
            (true, None) => {
                warnings.push("could not identify a capturable window for this app".into());
                None
            }
            (false, _) => None,
        };

        let id = NEXT_SNAPSHOT_ID.fetch_add(1, Ordering::Relaxed);
        let tree = crate::snapshot::render_tree(&nodes, opts.render);
        let node_count = nodes.len();
        let actionable_count = nodes.iter().filter(|n| n.is_actionable()).count();
        let window_title = window.as_ref().and_then(|w| w.title.clone());

        self.snapshots.insert(
            info.pid,
            Snapshot {
                id,
                nodes,
                window: window.clone(),
                taken_at: Instant::now(),
                process_key: key,
                scoped: opts.scope.is_some(),
                limits: opts.limits,
                complete,
                acted_on: false,
                popups: popups.clone(),
            },
        );

        Ok(AppState {
            popups,
            app: info,
            snapshot_id: id,
            tree,
            node_count,
            actionable_count,
            window_title,
            window_id: window.as_ref().map(|w| w.id),
            window_frame: window.map(|w| w.frame).or(ax_frame),
            screenshot,
            warnings,
        })
    }

    /// The outline of the snapshot this app already has, plus the window it
    /// described, rendered the same way the post-action read will be.
    ///
    /// `Err` with the reason when the existing snapshot is not a fair basis for
    /// a diff, so the caller can say why instead of emitting a diff that is
    /// arithmetically correct and completely useless. Both refusals were
    /// measured on KakaoTalk: a scoped walk describes one subtree, and a walk
    /// capped below the window's size describes the window partially, so in
    /// either case a later default walk reports hundreds of lines as new and
    /// buries the handful that are.
    pub(super) fn rendered_current_tree(
        &self,
        query: &str,
    ) -> std::result::Result<(String, Option<u32>), &'static str> {
        let info = apps::resolve_app(query)
            .map_err(|_| "this app could not be resolved before the action")?;
        let snap = self
            .snapshots
            .get(&info.pid)
            .ok_or("this app had no previous snapshot to diff against")?;
        diff_basis(snap)?;
        Ok((
            crate::snapshot::render_tree(&snap.nodes, post_action_render()),
            snap.window.as_ref().map(|w| w.id),
        ))
    }

    /// Re-walk the window after an action and diff it against `before`.
    pub(super) fn read_state_after(
        &mut self,
        query: &str,
        before: std::result::Result<(String, Option<u32>), &'static str>,
    ) -> Option<PostActionState> {
        let opts = StateOptions {
            include_screenshot: false,
            render: post_action_render(),
            limits: post_action_limits(),
            ..StateOptions::default()
        };
        // A failed re-read is reported, not swallowed. Returning `None` here made
        // it indistinguishable from `return_state: false`, so a caller could not
        // tell "the window is gone" from "you did not ask".
        let state = match self.get_app_state(query, opts) {
            Ok(state) => state,
            Err(e) => {
                return Some(PostActionState {
                    snapshot_id: None,
                    diff: None,
                    note: Some(format!(
                        "the action ran, but the window could not be read afterwards, so its                          effect is unobserved: {e}"
                    )),
                    node_count: 0,
                });
            }
        };

        // Only subtract trees that describe the same window. An action that
        // opens or switches windows — a chat row that opens a conversation —
        // leaves nothing meaningful to subtract, and presenting a whole new tree
        // as a change set would bury the answer instead of giving it.
        let after_window = apps::resolve_app(query)
            .ok()
            .and_then(|info| self.snapshots.get(&info.pid))
            .and_then(|s| s.window.as_ref().map(|w| w.id));
        // Two unknown ids are not a match. Without Screen Recording no window can
        // be identified at all, and treating `None == None` as "same window" made
        // the diff subtract two entirely different windows and call the result a
        // change set.
        let comparable =
            before.and_then(
                |(tree, before_window)| match (before_window, after_window) {
                    (Some(before_id), Some(after_id)) if before_id == after_id => Ok(tree),
                    (Some(_), Some(_)) => Err(
                        "the window this app is showing is not the one the previous snapshot \
                     described, so there is nothing to diff against",
                    ),
                    _ => Err(
                        "this app's window could not be identified, so there is no evidence the \
                     re-read describes the same window as the previous snapshot (a missing \
                     Screen Recording grant is the usual reason)",
                    ),
                },
            );

        Some(PostActionState {
            snapshot_id: Some(state.snapshot_id),
            diff: comparable
                .as_ref()
                .ok()
                .map(|b| crate::snapshot::diff_trees(b, &state.tree)),
            note: comparable.err().map(|why| {
                format!("{why}. The full tree is at the snapshot_id above; call get_app_state to read it")
            }),
            node_count: state.node_count,
        })
    }

    pub(super) fn find(&mut self, query: &str, needle: &str, limit: usize) -> Result<FindResult> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;

        // Search the snapshot the agent is already holding, so the indices it
        // gets back stay valid against the state it has seen. Walk afresh when
        // there is nothing to search, or when an action has happened since —
        // a search of the pre-action tree is an answer about a window that is
        // no longer on screen.
        let snapshot_id = match self.snapshots.get(&info.pid) {
            Some(s) if !s.acted_on => s.id,
            _ => {
                let opts = StateOptions {
                    include_screenshot: false,
                    ..Default::default()
                };
                self.get_app_state(query, opts)?.snapshot_id
            }
        };

        let snap = self
            .snapshots
            .get(&info.pid)
            .ok_or_else(|| CoreError::NoSnapshot {
                app: info.name.clone(),
            })?;

        let hits = match_nodes(&snap.nodes, needle, limit);
        Ok(FindResult {
            snapshot_id,
            total: hits.len(),
            lines: hits,
            searched: snap.nodes.len(),
        })
    }

    pub(super) fn wait_for(
        &mut self,
        query: &str,
        needle: &str,
        want: Presence,
        timeout_ms: u64,
    ) -> Result<WaitOutcome> {
        cua_ax::require_trusted()?;
        let deadline = Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let opts = StateOptions {
            include_screenshot: false,
            ..Default::default()
        };

        let mut polls = 0u32;
        loop {
            polls += 1;
            let state = self.get_app_state(query, opts)?;
            let present = state.tree.contains(needle);
            if present == want.wants_present() {
                return Ok(WaitOutcome {
                    satisfied: true,
                    polls,
                    snapshot_id: state.snapshot_id,
                    elapsed_ms: elapsed_ms_until(deadline, timeout_ms),
                });
            }
            if Instant::now() >= deadline {
                return Ok(WaitOutcome {
                    satisfied: false,
                    polls,
                    snapshot_id: state.snapshot_id,
                    elapsed_ms: timeout_ms,
                });
            }
            // A full tree walk per poll is not cheap, so the floor here is what
            // keeps `wait_for` from becoming a busy loop that starves the app it
            // is watching.
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    pub(super) fn overlay_target(&self, pid: libc::pid_t) -> Option<(u32, libc::pid_t)> {
        self.snapshots
            .get(&pid)
            .and_then(|snapshot| snapshot.window.as_ref())
            .map(|window| (window.id, pid))
    }

    /// A cheap proxy for "did the UI move".
    ///
    /// Deliberately not a second full tree walk: that would double the cost of
    /// every action to answer a question the agent can answer better by taking a
    /// fresh snapshot when it actually needs one. The focused element's identity
    /// plus the window title catches the common cases — a dialog opened, focus
    /// moved, a tab switched — and honestly reports `false` otherwise rather
    /// than claiming success it cannot see.
    pub(super) fn window_fingerprint(&self, pid: libc::pid_t) -> Option<String> {
        self.focus_probe(pid).fingerprint
    }

    /// The state an action will be measured against.
    ///
    /// The pop-up set is seeded from the last `get_app_state`, which is free —
    /// that read already enumerated windows. Paths that re-enumerate just before
    /// posting overwrite it with something fresher via [`Watch::with_windows`].
    pub(super) fn watch(&self, pid: libc::pid_t) -> Watch {
        Watch {
            fingerprint: self.window_fingerprint(pid),
            popups: self
                .snapshots
                .get(&pid)
                .map(|snap| snap.popups.iter().map(|p| p.id).collect()),
        }
    }

    /// One read of the app's focus state, serving both the `ui_changed`
    /// fingerprint and the focus check.
    ///
    /// The fingerprint already had to read `AXFocusedUIElement`; it just threw
    /// the element away and kept a string built from its role and title. That
    /// string is why a keystroke landing in a *different field of the same
    /// app* leaves the fingerprint identical — two text fields of one window
    /// usually share a role and have no title, so the comparison cannot see
    /// the difference and the action reports `ui_changed: false` while
    /// something really did change. Keeping the element itself costs nothing
    /// extra and answers the question the fingerprint cannot.
    pub(super) fn focus_probe(&self, pid: libc::pid_t) -> FocusProbe {
        let app = Element::for_pid(pid);
        let focused = app.element(cua_ax::attr::FOCUSED_UI_ELEMENT);
        let title = app
            .element(cua_ax::attr::FOCUSED_WINDOW)
            .and_then(|w| w.string(cua_ax::attr::TITLE));
        let fingerprint = format!(
            "{}|{}|{}",
            title.unwrap_or_default(),
            focused.as_ref().and_then(|f| f.role()).unwrap_or_default(),
            focused
                .as_ref()
                .and_then(|f| f.string(cua_ax::attr::TITLE))
                .unwrap_or_default()
        );
        // Every field empty means the app told us nothing — not that it is in
        // a particular state. Returning `Some("||")` here would make two such
        // reads compare equal and manufacture an `Unchanged` out of silence.
        let fingerprint = (fingerprint != "||").then_some(fingerprint);
        FocusProbe {
            focused,
            fingerprint,
        }
    }

    /// Try to move accessibility focus to `el`, then say where focus actually
    /// is — the two halves of [`FocusCheck`].
    ///
    /// Called after the `AXFocused` write and before any event is posted, so
    /// what it reports is the state the keystrokes were about to be delivered
    /// into rather than a reconstruction afterwards.
    pub(super) fn check_focus(
        &self,
        pid: libc::pid_t,
        el: &Element,
        write: std::result::Result<(), cua_ax::AxError>,
    ) -> FocusCheck {
        let focused = self.focus_probe(pid).focused;
        let addressed_is_focused = focused.as_ref().map(|f| f == el);
        let focused_instead = match addressed_is_focused {
            Some(false) => focused.as_ref().map(describe_element),
            _ => None,
        };
        FocusCheck {
            state: classify_focus(addressed_is_focused),
            focus_write_accepted: write.is_ok(),
            focus_write_error: write.err().map(|e| e.to_string()),
            focused_instead,
        }
    }

    /// How long to wait for an ordinary action's effect to become readable.
    ///
    /// Unchanged from the fixed sleep this replaced, so no existing action got
    /// slower: accessibility reflects most changes within a frame or two, and the
    /// deadline is only spent in full when nothing changed at all.
    const SETTLE_MS: u64 = 120;

    /// How often to re-read while waiting. One 60 Hz frame is the smallest interval
    /// at which a change could plausibly become visible, and polling faster would
    /// spend AX round trips to learn nothing.
    const SETTLE_POLL_MS: u64 = 16;

    /// Wait for the app to settle, then say what moved.
    ///
    /// Two observations, not one. The fingerprint answers "did the window this
    /// action addressed change", and the window list answers "did the app put
    /// something new on screen that is not in that window at all" — which is the
    /// question the fingerprint has never been able to answer, because a menu
    /// opening changes neither the focused element nor the window title.
    ///
    /// The enumeration is deliberately after the settle rather than immediately
    /// after the event: KakaoTalk's menu window appeared ~50 ms after the click,
    /// so an enumeration racing the event would have missed it and reported the
    /// same "nothing happened" this is here to stop. It costs p50 ~28 ms on top
    /// of the 120 ms, and it is the only window enumeration this adds per
    /// action.
    pub(super) fn changed_since(&self, pid: libc::pid_t, before: Watch) -> Settled {
        self.settle(pid, before, Self::SETTLE_MS)
    }

    /// Why there is no patient variant of this, though the numbers invited one.
    ///
    /// §10 records that a menu item's effect becomes readable 50 ms to 1.7 s
    /// after the press — up to fourteen times [`Inner::SETTLE_MS`] — which reads
    /// like a deadline that is simply too short. A 2 s deadline was built and
    /// measured, and it changed nothing: pressing TextEdit's Show Tab Bar
    /// reported `Unchanged` after waiting the whole 2 198 ms, while the *next*
    /// call proved the press had worked, because the item had renamed itself to
    /// Hide Tab Bar.
    ///
    /// The fingerprint reads the focused window's title and the focused
    /// element's role and label. Showing a tab bar changes none of them, so no
    /// amount of waiting makes it visible — the limit is *what* is compared, not
    /// *when*. The 1.7 s figure came from a probe watching the pressed item's own
    /// attributes, which is a different observation from this one.
    ///
    /// So the deadline stayed short, and the honest fix for a menu action is the
    /// one §10 already names: re-read the element, or read the menu again and
    /// look at the row's own title and mark. `menu_bar` returns both for exactly
    /// this reason.
    /// Poll the fingerprint until it moves, or until `deadline_ms` runs out.
    ///
    /// The poll replaced a fixed sleep, and it is strictly better in both
    /// directions: a change that lands in one frame is reported after one frame
    /// instead of after the whole window, and a change that takes ten frames is
    /// still seen. Only the no-change case pays the deadline, and it has to —
    /// "nothing happened" is exactly the claim that cannot be made early.
    pub(super) fn settle(&self, pid: libc::pid_t, before: Watch, deadline_ms: u64) -> Settled {
        let started = std::time::Instant::now();
        let deadline = std::time::Duration::from_millis(deadline_ms);
        let mut after;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(Self::SETTLE_POLL_MS));
            after = self.window_fingerprint(pid);
            // A difference is final: nothing later can un-change it, and waiting
            // to confirm would only add latency to the successful case.
            if after.is_some() && after != before.fingerprint {
                break;
            }
            if started.elapsed() >= deadline {
                break;
            }
        }
        let focus_verdict = match (before.fingerprint, after) {
            // Either end unreadable and the comparison is meaningless. Say so
            // instead of picking the answer that happens to be shorter.
            (None, _) | (_, None) => Observed::Unknown,
            (Some(a), Some(b)) if a == b => Observed::Unchanged,
            _ => Observed::Changed,
        };

        let windows = cua_capture::list_windows().unwrap_or_default();
        let popups = transient_popups(&windows, pid, before.popups.as_deref());
        // A window appearing or vanishing above the app's content is an
        // observable change by itself, whatever the fingerprint says. Both
        // directions count: a menu that closed is as much of an event as one
        // that opened, and reporting only the first would make `escape` look
        // like a no-op.
        let popups_moved = match &before.popups {
            Some(ids) => {
                let now: Vec<u32> = popups.iter().map(|p| p.id).collect();
                let mut before_ids = ids.clone();
                before_ids.sort_unstable();
                let mut now = now;
                now.sort_unstable();
                before_ids != now
            }
            None => false,
        };

        Settled {
            changed: if popups_moved {
                Observed::Changed
            } else {
                focus_verdict
            },
            popups,
        }
    }
}
