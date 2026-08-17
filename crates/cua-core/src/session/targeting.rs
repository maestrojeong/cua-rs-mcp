//! `Inner` methods for targeting responsibilities.

use super::*;

impl Inner {
    /// Turn a [`Target`] into a concrete element, validating snapshot identity.
    pub(super) fn resolve(
        &self,
        query: &str,
        target: &Target,
    ) -> Result<(AppInfo, Element, String)> {
        let info = apps::resolve_app(query)?;
        match *target {
            Target::Index {
                index,
                snapshot_id,
                ref expected_role,
            } => {
                let snap = self
                    .snapshots
                    .get(&info.pid)
                    .ok_or_else(|| CoreError::NoSnapshot {
                        app: info.name.clone(),
                    })?;

                if snap.process_key != ProcessKey::for_pid(info.pid) {
                    return Err(CoreError::ProcessReplaced {
                        app: info.name.clone(),
                    });
                }

                // Only checked when the caller supplied an id. Requiring it
                // would break the simple "read then act in one turn" flow that
                // is the overwhelmingly common case; honoring it when present
                // lets a careful caller get a hard guarantee.
                if let Some(given) = snapshot_id {
                    if given != snap.id {
                        return Err(CoreError::StaleSnapshot {
                            index,
                            given,
                            current: snap.id,
                        });
                    }
                }

                let node = snap.nodes.get(index).ok_or(CoreError::BadIndex {
                    index,
                    count: snap.nodes.len(),
                })?;
                if let Some(expected) = expected_role {
                    if &node.role != expected {
                        return Err(CoreError::TokenRoleMismatch {
                            index,
                            expected: expected.clone(),
                            found: node.role.clone(),
                        });
                    }
                }

                Ok((info, node.element.clone(), describe_node(node)))
            }
            // Resolved against the snapshot's own geometry, not with
            // `AXUIElementCopyElementAtPosition`. On the app element of a
            // *background* app that API answered `AXMenuBar` for every point
            // tried — measured on one app, but every app cua-rs drives is
            // backgrounded by design — so it silently retargeted those
            // coordinate click at the menu bar, and the failure surfaced as an
            // unrelated "the AX element and window snapshot drifted apart".
            // The snapshot already carries each element's frame, so the
            // hit-test needs no help from AX.
            Target::Point { x, y, snapshot_id } => {
                let snap = self
                    .snapshots
                    .get(&info.pid)
                    .ok_or_else(|| CoreError::NoSnapshot {
                        app: info.name.clone(),
                    })?;

                if snap.process_key != ProcessKey::for_pid(info.pid) {
                    return Err(CoreError::ProcessReplaced {
                        app: info.name.clone(),
                    });
                }

                // Same opt-in generation guard `Target::Index` gets, and more
                // load-bearing here: a stale index can be caught by the role
                // or the text it used to carry, while a stale pixel looks
                // exactly like a fresh one.
                if let Some(given) = snapshot_id {
                    if given != snap.id {
                        return Err(CoreError::StaleCoordinate {
                            app: info.name.clone(),
                            given,
                            current: snap.id,
                            x: f64::from(x),
                            y: f64::from(y),
                        });
                    }
                }

                // A coordinate is only meaningful against current geometry. An
                // index keeps working after an action because it names an element
                // and the element is re-read; a point names a *place*, and the
                // element that occupied it may have moved. Opening a disclosure
                // with `return_state: false` and then clicking the same point
                // would otherwise resolve to whatever used to be there and act on
                // that element wherever it is now.
                if snap.acted_on {
                    return Err(CoreError::StalePointGeometry {
                        app: info.name.clone(),
                    });
                }

                let node = hit_test(&snap.nodes, x, y).ok_or(CoreError::NoElementAtPoint {
                    app: info.name.clone(),
                    x,
                    y,
                })?;
                let desc = format!("{} at ({x}, {y})", describe_node(node));
                Ok((info, node.element.clone(), desc))
            }
        }
    }

    /// Run one action and, when asked, re-read the window and attach what
    /// changed.
    ///
    /// The re-read has to happen here rather than in a follow-up call for two
    /// reasons. It is the same hop on the AX worker thread, so it cannot
    /// interleave with another caller's action; and the *pre*-action tree has to
    /// be rendered before the action runs, because the action replaces the
    /// snapshot it would have been rendered from.
    ///
    /// Failing to re-read is not an error. The action already happened, and
    /// reporting it as a failure because the follow-up read did not work would
    /// invite a caller to retry something that already took effect.
    /// The snapshot's own description of the element an action is aimed at.
    ///
    /// Read from the stored snapshot rather than from the live element: the
    /// destructive heuristic has to judge the control the *caller* chose, which
    /// is the one the tree showed them. `None` when nothing can be resolved,
    /// which the gate treats as "unknown" rather than "harmless"; the action's
    /// own resolution then reports the real reason. A live default-button or
    /// parent read that fails is different: safety cannot establish what Return
    /// would press, so that AX error is returned before the action can run.
    pub(super) fn safety_candidate(
        &self,
        query: &str,
        target: &Target,
        key: Option<&str>,
    ) -> Result<Option<crate::safety::Candidate>> {
        let Some(info) = apps::resolve_app(query).ok() else {
            return Ok(None);
        };
        let Some(snap) = self.snapshots.get(&info.pid) else {
            return Ok(None);
        };
        let node = match *target {
            Target::Index { index, .. } => {
                let Some(node) = snap.nodes.get(index) else {
                    return Ok(None);
                };
                node
            }
            // `snapshot_id` is the staleness guard the action itself enforces;
            // classifying a label needs only the geometry, so it is ignored
            // here rather than duplicating the refusal.
            Target::Point { x, y, .. } => {
                let Some(node) = hit_test(&snap.nodes, x, y) else {
                    return Ok(None);
                };
                node
            }
        };
        // The snapshot is flat and every node names its parent, so the whole
        // tree the context rule needs is a borrowed projection of it. Which
        // ancestors count, how far up the search goes and what text inside them
        // is evidence are all decided in `safety`; this end only hands over the
        // shape.
        let projection: Vec<crate::safety::ContextNode<'_>> = snap
            .nodes
            .iter()
            .map(|n| crate::safety::ContextNode {
                parent: n.parent,
                role: &n.role,
                subrole: n.subrole.as_deref(),
                label: n.label.as_deref(),
                value: n.value.as_deref(),
                help: n.help.as_deref(),
                settable: n.settable,
            })
            .collect();
        let context = crate::safety::decision_context(&projection, node.index);
        let caption = crate::safety::caption(&projection, node.index);

        let mut candidate = crate::safety::Candidate {
            role: node.role.clone(),
            label: node.label.clone(),
            value: node.value.clone(),
            help: node.help.clone(),
            settable: node.settable,
            caption,
            description: describe_node(node),
            context,
        };

        // Return does not land where it was aimed. Inside a decision context it
        // presses that context's default button, so that is the control the gate
        // has to judge — see `safety::key_activates_default_button`. The question
        // is unchanged, because both controls are answers to the same one; only
        // the answer being given is different from the one the caller named.
        //
        // Read live rather than from the snapshot: `AXDefaultButton` is a
        // reference the window publishes, and following it is exact, where
        // guessing which snapshot node it points at would be a second heuristic
        // inside a gate that exists to avoid one. This runs on the native
        // thread, which is the only place an `AXUIElement` may be touched.
        // Attempted whenever the key is Return, not only when a decision context
        // was found above the aimed element. Aiming at the dialog *window*
        // itself has no context above it — the window is the context — and the
        // first live run of this gate pressed a real "Delete" that way. If
        // nothing up the chain publishes a default button, which is every
        // ordinary window, this resolves to `None` and changes nothing.
        if key.is_some_and(crate::safety::key_activates_default_button) {
            if let Some(button) = default_button_of_ancestor(&node.element)? {
                candidate.substitute_answer(
                    button.role().unwrap_or_default(),
                    button.label(),
                    describe_element(&button),
                );
            }
        }

        Ok(Some(candidate))
    }

    pub(super) fn acting<F>(
        &mut self,
        query: &str,
        gate: crate::safety::Gate,
        return_state: bool,
        act: F,
    ) -> Result<ActionResult>
    where
        F: FnOnce(&mut Self) -> Result<ActionResult>,
    {
        // Every gate, once, at the one place every action passes through.
        // Putting it here rather than in each action means a tool added later
        // is gated by default instead of by remembering. An app that will not
        // resolve is left to the action, which reports that better.
        if let Ok(info) = apps::resolve_app(query) {
            let candidate = match gate.target() {
                Some(t) => self.safety_candidate(query, t, gate.key())?,
                // A menu bar row describes itself; there is no snapshot index
                // to look it up by, and the row's own title is exactly what the
                // classifier wants to read.
                None => gate.labelled_candidate().cloned(),
            };
            crate::safety::guard(&info, &self.human, &gate, candidate.as_ref())?;
        }

        let before = if return_state {
            Some(self.rendered_current_tree(query))
        } else {
            None
        };

        let mut result = act(self)?;

        match before {
            Some(before) => result.state = self.read_state_after(query, before),
            // No re-read, so the snapshot still describes the window as it was
            // before the action. Anything that would otherwise answer from it
            // has to know that.
            None => {
                if let Ok(info) = apps::resolve_app(query) {
                    if let Some(snap) = self.snapshots.get_mut(&info.pid) {
                        snap.acted_on = true;
                    }
                }
            }
        }
        Ok(result)
    }

    /// Refuse a window-local coordinate that was chosen from a snapshot this
    /// app has since replaced.
    ///
    /// The window id already proves the caller is aiming at a window it has
    /// *seen*; it does not prove the caller is aiming at the state it saw. A
    /// window id outlives any number of re-reads, so without this a point
    /// picked off screenshot 3 is accepted verbatim against the window as it
    /// looks at snapshot 9 — same window, different contents, and a pixel that
    /// now covers something else entirely. The snapshot id is the generation
    /// number that distinguishes the two, and it is the same one
    /// `element_token` pins an index with.
    ///
    /// Opt-in, for the same reason the index guard is: the common flow is
    /// read-then-act inside one turn, and requiring the id would add a failure
    /// mode where there is no risk. A caller that intends to act on a
    /// coordinate it decided earlier is exactly the caller who should pass it.
    pub(super) fn check_coordinate_generation(
        &self,
        info: &AppInfo,
        given: Option<u64>,
        at: (f64, f64),
    ) -> Result<()> {
        let Some(given) = given else {
            return Ok(());
        };
        let current = self
            .snapshots
            .get(&info.pid)
            .ok_or_else(|| CoreError::NoSnapshot {
                app: info.name.clone(),
            })?
            .id;
        if given != current {
            return Err(CoreError::StaleCoordinate {
                app: info.name.clone(),
                given,
                current,
                x: at.0,
                y: at.1,
            });
        }
        Ok(())
    }

    /// The one live window a pointer gesture may be pinned to: the window this
    /// app's most recent `get_app_state` read, re-enumerated now.
    ///
    /// Re-enumerated rather than trusted from the snapshot for the same reason
    /// [`Inner::click_in_window`] does it: a pid-addressed event carrying a
    /// stale or recycled window id is precisely the thing that must not be
    /// sent.
    pub(super) fn live_snapshot_window(
        &self,
        info: &AppInfo,
    ) -> std::result::Result<WindowInfo, String> {
        let wid = self
            .snapshots
            .get(&info.pid)
            .and_then(|snap| snap.window.as_ref())
            .map(|w| w.id)
            .ok_or_else(|| {
                "no verified window has been read for this app. Call get_app_state first (and grant Screen Recording, which is what identifies the window)".to_string()
            })?;
        let live_windows = cua_capture::list_windows()
            .map_err(|e| format!("could not revalidate the window before input: {e}"))?;
        live_window_for_pid_click(&live_windows, wid, info.pid)
    }

    /// Resolve a [`PointerLocation`] to a screen point inside `live`.
    pub(super) fn aim(
        &self,
        query: &str,
        info: &AppInfo,
        live: &WindowInfo,
        loc: &PointerLocation,
        snapshot_id: Option<u64>,
    ) -> std::result::Result<PointerAim, String> {
        let (point, desc, from_element) = match loc {
            PointerLocation::Element(target) => {
                let (_, el, desc) = self.resolve(query, target).map_err(|e| e.to_string())?;
                let point = element_point(&el).ok_or_else(|| {
                    format!("{desc} publishes neither AXActivationPoint nor AXFrame, so there is no point to aim at")
                })?;
                (point, desc, true)
            }
            PointerLocation::WindowPoint { x, y } => {
                // The element form gets this check inside `resolve`; the raw
                // form has no element to hang it on, so it happens here.
                self.check_coordinate_generation(info, snapshot_id, (*x, *y))
                    .map_err(|e| e.to_string())?;
                if *x < 0.0 || *y < 0.0 {
                    return Err(format!(
                        "window-local coordinates are measured from the window's top-left corner, so neither of ({x:.0}, {y:.0}) can be negative"
                    ));
                }
                (
                    (live.frame.origin.x + x, live.frame.origin.y + y),
                    format!("window-local ({x:.0}, {y:.0}) — no element, the caller aimed this"),
                    false,
                )
            }
        };
        screen_point_inside(live, point.0, point.1).map_err(|frame| {
            format!(
                "({:.0}, {:.0}) is outside the current frame of window {} ({frame}); a gesture cannot cross a window boundary, and cua-rs will not guess which window you meant",
                point.0, point.1, live.id
            )
        })?;
        Ok(PointerAim {
            point,
            window_local: (point.0 - live.frame.origin.x, point.1 - live.frame.origin.y),
            desc,
            from_element,
        })
    }
}
