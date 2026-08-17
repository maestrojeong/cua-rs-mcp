//! `Inner` methods for menu responsibilities.

use super::*;

impl Inner {
    pub(super) fn menu_bar(
        &mut self,
        query: &str,
        path: &str,
    ) -> Result<crate::menubar::MenuListing> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;
        let app = cua_ax::Element::for_pid(info.pid);
        let steps = crate::menubar::menu_path_steps(path);
        crate::menubar::walk(&app, &steps)
            .map(|(listing, _)| listing)
            .map_err(|e| CoreError::MenuPath {
                app: info.name,
                reason: e.to_string(),
            })
    }

    pub(super) fn press_menu_bar(
        &mut self,
        query: &str,
        path: &str,
        return_state: bool,
        confirm_destructive: bool,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let info = apps::resolve_app(query)?;
        let app_el = cua_ax::Element::for_pid(info.pid);
        let steps = crate::menubar::menu_path_steps(path);
        if steps.is_empty() {
            return Err(CoreError::MenuPath {
                app: info.name,
                reason: "no menu path given; pass one like `Edit > Paste`".into(),
            });
        }
        let name = |e: crate::menubar::MenuWalkError| CoreError::MenuPath {
            app: info.name.clone(),
            reason: e.to_string(),
        };
        let (listing, landed) = crate::menubar::walk(&app_el, &steps).map_err(name)?;
        let Some(item) = landed else {
            return Err(name(crate::menubar::MenuWalkError::IsSubmenu {
                path: listing.path.clone(),
                children: listing.items.iter().map(|i| i.title.clone()).collect(),
            }));
        };
        // A row that owns a submenu is not an action. Refusing is better than
        // pressing it: `AXPress` on such a row opens a menu nobody can see.
        if item
            .children()
            .iter()
            .any(|c| c.role().as_deref() == Some("AXMenu"))
        {
            return Err(name(crate::menubar::MenuWalkError::IsSubmenu {
                path: listing.path.clone(),
                children: listing.items.iter().map(|i| i.title.clone()).collect(),
            }));
        }

        let title = item.label().unwrap_or_default();
        let described = format!("menu item `{}`", listing.path);
        // The label gate, on the row's own title. A menu bar reaches Quit and
        // Log Out in two steps, so this is not a formality.
        let gate = crate::safety::Gate::labelled(
            "menu_bar",
            crate::safety::Candidate {
                role: "AXMenuItem".into(),
                label: Some(title),
                description: described.clone(),
                ..Default::default()
            },
        )
        .confirmed(confirm_destructive);

        let enabled = item.bool(cua_ax::attr::ENABLED).unwrap_or(false);
        let path_for_result = listing.path.clone();
        self.acting(query, gate, return_state, move |i| {
            // Reported rather than refused: a menu bar validates against the
            // live responder, and an item that reads disabled a millisecond
            // before the press is evidence, not proof. Pressing it is a no-op
            // either way, and saying so is more useful than a refusal that
            // might be wrong.
            if !enabled {
                return Err(CoreError::MenuPath {
                    app: info.name.clone(),
                    reason: format!(
                        "menu item `{path_for_result}` is disabled right now, so pressing it \
                         would do nothing. A menu bar validates against the app's current \
                         focus and selection: click or select what the item acts on first, \
                         then read the menu again"
                    ),
                });
            }
            let before = i.watch(info.pid);
            item.perform(cua_ax::action::PRESS)?;
            let changed = i.changed_since(info.pid, before);
            Ok(ActionResult::ax_at(
                format!("AXPress on menu item `{path_for_result}`"),
                described.clone(),
                changed,
                None,
            )
            .with_overlay_target(i.overlay_target(info.pid)))
        })
    }

    pub(super) fn perform_action(
        &mut self,
        query: &str,
        target: Target,
        action: &str,
    ) -> Result<ActionResult> {
        cua_ax::require_trusted()?;
        let (info, el, desc) = self.resolve(query, &target)?;
        let available = el.actions();
        if !available.iter().any(|a| a == action) {
            // List what the element *does* support: an agent that guessed a verb
            // can fix itself in one step instead of retrying blindly.
            return Err(CoreError::Ax(cua_ax::AxError::Unsupported {
                what: "action",
                name: format!("{action} (this element supports {available:?})"),
            }));
        }
        let before = self.watch(info.pid);
        el.perform(action)?;
        let changed = self.changed_since(info.pid, before);
        Ok(
            ActionResult::ax_at(action, desc, changed, element_point(&el))
                .with_overlay_target(self.overlay_target(info.pid)),
        )
    }
}
