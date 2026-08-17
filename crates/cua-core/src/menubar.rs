//! The one menu accessibility does describe.
//!
//! A pop-up menu a click opens has no accessibility representation at all
//! (DESIGN §10), and a row inside it can only be activated by its own keyboard
//! shortcut — which means a row that has no shortcut cannot be activated. That
//! is measured, and it is not fixable from here: the arrow keys reach the
//! menu's tracking loop and move the highlight, but Return is consumed without
//! activating anything.
//!
//! The app's *menu bar* is the exception, and it is a large one. `AXMenuBar` is
//! published in full — every menu, every submenu, every row, each with
//! `AXPress`, an `AXEnabled` that tracks the real responder, its key equivalent
//! as data rather than as pixels, and its checkmark. Many of the rows a pop-up
//! draws are the same `NSMenuItem`s the menu bar draws, so a shortcut-less
//! pop-up row is reachable after all — through its menu-bar twin, when it has
//! one.
//!
//! Measured on TextEdit, with another app frontmost throughout: `편집 > 변형 >
//! 대문자로 만들기` (Make Upper Case, no key equivalent, and one of the rows the
//! text view's context menu also draws) turned a selected `bravo` into `BRAVO`,
//! and `소문자로 만들기` turned it back. The item reported `enabled: false` with
//! no selection and `enabled: true` with one, so the validation is the app's
//! real one and not a stale cache.
//!
//! The walk is by title, not by index, because a menu's indices move with its
//! separators and its dynamically inserted rows while its titles are what the
//! caller read.

use cua_ax::{attr, Element};

/// One row of a menu bar menu.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    /// Title as drawn. Empty for a separator, which is kept in the listing
    /// rather than filtered out: a caller comparing what cua-rs reports against
    /// what they can see should find the same rows in the same order.
    pub title: String,
    /// `>`-separated path from the menu bar, ready to be passed back in.
    pub path: String,
    /// Whether the app says this row can be chosen *right now*. A menu bar
    /// validates against the current first responder, so this moves with the
    /// selection and the focus, and pressing a disabled row does nothing.
    pub enabled: bool,
    pub has_submenu: bool,
    /// The row's key equivalent in `press_key`'s own vocabulary — `cmd+i`,
    /// `cmd+alt+,` — or `None` when it has none. See [`menu_shortcut`].
    pub shortcut: Option<String>,
    /// The mark drawn at the left: `✓` on a checked toggle. This is how a
    /// toggle's state is read back without looking at pixels.
    pub mark: Option<String>,
}

/// The rows at one level of the menu bar, plus the path that named them.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuListing {
    /// The path that was walked, `""` for the top level.
    pub path: String,
    pub items: Vec<MenuItem>,
}

/// What went wrong walking a path.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuWalkError {
    /// The app's AX bridge failed while the menu hierarchy was being read.
    /// Kept separate from absence so a busy app is never described as one that
    /// does not publish a menu bar or the requested row.
    Ax(cua_ax::AxError),
    /// The application element publishes no `AXMenuBar` at all — a background
    /// agent, or an app that has not finished launching.
    NoMenuBar,
    /// No row with this title at this level. Carries the titles that *are*
    /// there, because a menu path is nearly always misspelled rather than
    /// missing, and one round trip should be enough to fix it.
    NoSuchItem {
        step: String,
        at: String,
        available: Vec<String>,
    },
    /// The path names a row that owns a submenu, and the caller asked to press
    /// it. Opening a submenu is not an action; naming a row inside it is.
    IsSubmenu { path: String, children: Vec<String> },
}

impl std::fmt::Display for MenuWalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ax(e) => write!(f, "could not read this app's menu bar: {e}"),
            Self::NoMenuBar => write!(f, "this app publishes no menu bar"),
            Self::NoSuchItem {
                step,
                at,
                available,
            } => {
                let at = if at.is_empty() {
                    "the menu bar".to_string()
                } else {
                    format!("`{at}`")
                };
                write!(
                    f,
                    "no menu item titled {step:?} in {at}. It has: {}",
                    available
                        .iter()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!("{t:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Self::IsSubmenu { path, children } => write!(
                f,
                "`{path}` opens a submenu rather than doing anything; name a row inside it: {}",
                children
                    .iter()
                    .filter(|t| !t.is_empty())
                    .map(|t| format!("{t:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

impl From<cua_ax::AxError> for MenuWalkError {
    fn from(value: cua_ax::AxError) -> Self {
        Self::Ax(value)
    }
}

/// Split a `>`-separated menu path into its steps, ignoring surrounding space.
///
/// Empty for `""`, so the top level is the natural default rather than a case.
/// Pure and unit-tested.
pub fn menu_path_steps(path: &str) -> Vec<&str> {
    path.split('>')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Format a menu item's key equivalent the way [`crate::Cua::press_key`] spells
/// one, or `None` when the item has no shortcut.
///
/// `AXMenuItemCmdModifiers` is not the mask it looks like. It is the Carbon
/// menu modifier byte, in which **Command is the default and bit 3 turns it
/// off**: `0` means ⌘ alone, `1` adds Shift, `2` adds Option, `4` adds Control,
/// and `8` means "no Command". Reading it as an ordinary flags word gets ⌘ wrong
/// on every item that has one, which is nearly all of them.
///
/// `cmd_char` is the character the item draws — `"i"` for ⌘I. An empty or
/// missing one means no shortcut, and the modifiers are then meaningless.
///
/// The point of returning `press_key` syntax rather than `⌘I` is that the result
/// is directly usable: a caller can read a shortcut off the menu bar and press
/// it on a pop-up that draws the same row, without anyone recognising a glyph
/// from an image. DESIGN §9 rules out parsing key equivalents out of *pixels*
/// for exactly the reason this does not share — a misread `⌥⌘⌫` presses "leave
/// the chat room", and this is not reading.
///
/// Pure and unit-tested; touches no app.
pub fn menu_shortcut(cmd_char: Option<&str>, modifiers: Option<f64>) -> Option<String> {
    let key = cmd_char.map(str::trim).filter(|c| !c.is_empty())?;
    // A menu item with no key equivalent reports U+0000 rather than an absent
    // attribute in some apps, and a NUL is not a shortcut.
    if key.chars().all(|c| c.is_control()) {
        return None;
    }
    let bits = modifiers.unwrap_or(0.0) as u32;
    // Command first, then the order the glyphs are drawn in — `press_key`
    // ignores the order, but a caller comparing this against ⌃⌥⇧⌘ on screen
    // should not have to.
    let mut parts: Vec<&str> = Vec::with_capacity(5);
    if bits & 8 == 0 {
        parts.push("cmd");
    }
    if bits & 4 != 0 {
        parts.push("ctrl");
    }
    if bits & 2 != 0 {
        parts.push("alt");
    }
    if bits & 1 != 0 {
        parts.push("shift");
    }
    // `AXMenuItemCmdChar` reports ⌘Z as an uppercase `Z`. `parse_chord` is
    // case-insensitive so either spells the same chord, but the point of this
    // string is that it can be pasted straight into `press_key`, and
    // `cmd+shift+Z` invites a reader to wonder whether the case is load-bearing.
    let lowered = key.to_lowercase();
    parts.push(&lowered);
    Some(parts.join("+"))
}

/// Read one level of a menu, or press a leaf, by walking `AXMenuBar` along
/// `steps`.
///
/// Returns the element the path ended on together with the listing of the level
/// it lives in, so a caller can decide between describing it and pressing it
/// without walking twice.
pub(crate) fn walk(
    app: &Element,
    steps: &[&str],
) -> std::result::Result<(MenuListing, Option<Element>), MenuWalkError> {
    let mut level = app
        .element_checked(attr::MENU_BAR)?
        .ok_or(MenuWalkError::NoMenuBar)?;
    let mut walked = String::new();
    let mut landed: Option<Element> = None;

    for (depth, step) in steps.iter().enumerate() {
        // One read of the level, used both to find the step and to report what
        // was there instead. Two reads could disagree: a menu bar is live, and
        // an app is free to insert a row between them.
        let items = level_items(&level)?;
        let Some(hit) = items
            .iter()
            .find(|el| el.label().unwrap_or_default() == *step)
            .cloned()
        else {
            return Err(MenuWalkError::NoSuchItem {
                step: (*step).to_string(),
                at: walked,
                available: items
                    .iter()
                    .map(|el| el.label().unwrap_or_default())
                    .collect(),
            });
        };
        walked = join(&walked, step);
        match submenu_of(&hit)? {
            // Descend, unless this is the last step: the caller may well have
            // meant "list this submenu", and pressing a row that opens one is
            // refused separately.
            Some(sub) if depth + 1 < steps.len() => level = sub,
            Some(sub) => {
                landed = Some(hit);
                level = sub;
            }
            None => {
                landed = Some(hit);
                return Ok((
                    MenuListing {
                        path: walked.clone(),
                        items: rows_of(&level, &parent_path(&walked))?,
                    },
                    landed,
                ));
            }
        }
    }

    Ok((
        MenuListing {
            path: walked.clone(),
            items: rows_of(&level, &walked)?,
        },
        landed,
    ))
}

/// The menu rows directly under `level`, described.
fn rows_of(level: &Element, path: &str) -> std::result::Result<Vec<MenuItem>, MenuWalkError> {
    level_items(level)?
        .iter()
        .map(|el| describe(el, path))
        .collect()
}

/// The child elements of `level` that are menu rows.
///
/// An `AXMenuBar`'s children are `AXMenuBarItem`s and an `AXMenu`'s are
/// `AXMenuItem`s; anything else at either level is structure and is dropped.
fn level_items(level: &Element) -> std::result::Result<Vec<Element>, MenuWalkError> {
    Ok(level
        .elements_checked(attr::CHILDREN)?
        .into_iter()
        .filter(|c| {
            matches!(
                c.role().as_deref(),
                Some("AXMenuBarItem") | Some("AXMenuItem")
            )
        })
        .collect())
}

/// The `AXMenu` a row owns, when it owns one. A row with a submenu has exactly
/// one `AXMenu` child; a leaf has none.
fn submenu_of(item: &Element) -> std::result::Result<Option<Element>, MenuWalkError> {
    Ok(item
        .elements_checked(attr::CHILDREN)?
        .into_iter()
        .find(|c| c.role().as_deref() == Some("AXMenu")))
}

fn describe(el: &Element, path: &str) -> std::result::Result<MenuItem, MenuWalkError> {
    let title = el.label().unwrap_or_default();
    Ok(MenuItem {
        path: join(path, &title),
        enabled: el.bool(attr::ENABLED).unwrap_or(false),
        has_submenu: submenu_of(el)?.is_some(),
        shortcut: menu_shortcut(
            el.string(attr::MENU_ITEM_CMD_CHAR).as_deref(),
            el.number(attr::MENU_ITEM_CMD_MODIFIERS),
        ),
        mark: el
            .string(attr::MENU_ITEM_MARK_CHAR)
            .filter(|m| !m.trim().is_empty()),
        title,
    })
}

fn join(path: &str, step: &str) -> String {
    if path.is_empty() {
        step.to_string()
    } else {
        format!("{path} > {step}")
    }
}

fn parent_path(path: &str) -> String {
    match path.rsplit_once('>') {
        Some((head, _)) => head.trim_end().to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_splits_on_chevrons_and_tolerates_spacing() {
        assert_eq!(menu_path_steps("Edit>Paste"), vec!["Edit", "Paste"]);
        assert_eq!(menu_path_steps(" Edit > Find > Find… "), {
            vec!["Edit", "Find", "Find…"]
        });
        assert!(menu_path_steps("").is_empty());
        assert!(menu_path_steps("  >  ").is_empty());
    }

    #[test]
    fn command_is_the_default_modifier_and_bit_three_removes_it() {
        // The whole reason this function exists: 0 is ⌘, not "no modifiers".
        assert_eq!(
            menu_shortcut(Some("i"), Some(0.0)).as_deref(),
            Some("cmd+i")
        );
        assert_eq!(menu_shortcut(Some("t"), None).as_deref(), Some("cmd+t"));
        assert_eq!(
            menu_shortcut(Some("f5"), Some(8.0)).as_deref(),
            Some("f5"),
            "bit 3 means the item has no Command in its shortcut"
        );
    }

    #[test]
    fn the_other_modifier_bits_read_in_press_key_order() {
        assert_eq!(
            menu_shortcut(Some(","), Some(2.0)).as_deref(),
            Some("cmd+alt+,")
        );
        assert_eq!(
            menu_shortcut(Some("s"), Some(1.0)).as_deref(),
            Some("cmd+shift+s")
        );
        assert_eq!(
            menu_shortcut(Some("a"), Some(4.0)).as_deref(),
            Some("cmd+ctrl+a")
        );
        assert_eq!(
            menu_shortcut(Some("z"), Some(1.0 + 2.0 + 4.0)).as_deref(),
            Some("cmd+ctrl+alt+shift+z")
        );
    }

    #[test]
    fn a_shortcut_character_is_reported_in_lower_case() {
        // ⌘Z arrives as an uppercase `Z`, and ⇧ is already carried by the
        // modifier bits, so upper-casing it here would say Shift twice.
        assert_eq!(
            menu_shortcut(Some("Z"), Some(0.0)).as_deref(),
            Some("cmd+z")
        );
        assert_eq!(
            menu_shortcut(Some("V"), Some(1.0 + 2.0)).as_deref(),
            Some("cmd+alt+shift+v")
        );
    }

    #[test]
    fn an_item_with_no_key_equivalent_has_no_shortcut() {
        // The case the whole investigation is about: these are the rows that
        // cannot be reached inside a pop-up, and the menu bar is their only way
        // in — so reporting a shortcut they do not have would be the worst
        // possible lie here.
        assert_eq!(menu_shortcut(None, Some(0.0)), None);
        assert_eq!(menu_shortcut(Some(""), Some(0.0)), None);
        assert_eq!(menu_shortcut(Some("  "), Some(0.0)), None);
        assert_eq!(
            menu_shortcut(Some("\u{0}"), Some(0.0)),
            None,
            "some apps report NUL rather than omitting the attribute"
        );
    }

    #[test]
    fn a_path_is_built_by_joining_and_a_parent_by_dropping_the_last_step() {
        assert_eq!(join("", "Edit"), "Edit");
        assert_eq!(join("Edit", "Find"), "Edit > Find");
        assert_eq!(parent_path("Edit > Find > Find…"), "Edit > Find");
        assert_eq!(parent_path("Edit"), "");
    }

    #[test]
    fn a_walk_error_names_what_was_there_instead() {
        let e = MenuWalkError::NoSuchItem {
            step: "Pastee".into(),
            at: "Edit".into(),
            available: vec!["Cut".into(), String::new(), "Paste".into()],
        };
        let rendered = e.to_string();
        assert!(rendered.contains("\"Pastee\""), "{rendered}");
        assert!(rendered.contains("`Edit`"), "{rendered}");
        assert!(rendered.contains("\"Paste\""), "{rendered}");
        assert!(
            !rendered.contains("\"\""),
            "separators are not suggestions: {rendered}"
        );
    }

    #[test]
    fn a_top_level_miss_says_the_menu_bar_rather_than_an_empty_name() {
        let e = MenuWalkError::NoSuchItem {
            step: "Edti".into(),
            at: String::new(),
            available: vec!["Edit".into()],
        };
        assert!(e.to_string().contains("the menu bar"), "{e}");
    }

    #[test]
    fn an_ax_failure_is_not_described_as_an_absent_menu_bar() {
        let e = MenuWalkError::Ax(cua_ax::AxError::CannotComplete);
        let rendered = e.to_string();
        assert!(rendered.contains("did not complete"), "{rendered}");
        assert!(!rendered.contains("publishes no menu bar"), "{rendered}");
    }

    #[test]
    fn pressing_a_submenu_is_refused_with_its_rows() {
        let e = MenuWalkError::IsSubmenu {
            path: "Edit > Transformations".into(),
            children: vec!["Make Upper Case".into()],
        };
        assert!(e.to_string().contains("Make Upper Case"), "{e}");
    }
}
