//! The general pasteboard — the one file in the workspace that names
//! `NSPasteboard`.
//!
//! Mechanism only. Whether a read may return its text, and whether a write may
//! happen at all, is decided in [`crate::safety`]; this module reads and writes
//! and reports what it saw. The split is the one `cua-hid::humanwatch` and
//! `safety.rs` already follow for the input tap, and for the same reason: "which
//! code can touch the clipboard" should be answerable from one manifest feature
//! and one file, not from a grep somebody has to trust.
//!
//! # Threading
//!
//! Nothing here spawns a thread or is called from one of its own. Every function
//! is invoked from a closure the session ships to the `cua-native` worker
//! (DESIGN.md §4), exactly like an AX call. That keeps clipboard calls
//! serialized with everything else, so a `clipboard_set` cannot interleave with
//! a `press_key cmd+v` that was meant to paste what it wrote.
//!
//! The worker has no autorelease pool of its own, and `stringForType:` and
//! `types` hand back autoreleased objects, so each call drains its own pool.
//!
//! # What reads cost on macOS 26
//!
//! macOS 26 added pasteboard privacy: a programmatic read of another app's
//! *contents* may raise a system alert, and the user can set this process to
//! always allow or always deny. [`access`] reports which, so "the clipboard is
//! empty" and "macOS is refusing to show it to us" can be told apart. Reading
//! the type list and the change count do not read contents.

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSPasteboardAccessBehavior, NSPasteboardTypeString};
use objc2_foundation::{NSObjectProtocol, NSString};

/// The UTI `NSPasteboardTypeString` resolves to. Kept as a constant so policy
/// code and tests can name it without touching AppKit.
pub const STRING_TYPE: &str = "public.utf8-plain-text";

/// The nspasteboard.org marker a writer sets to say "this is a secret — do not
/// display or record it". Written by password managers (1Password, KeePassXC,
/// Bitwarden and others) alongside the password itself.
pub const CONCEALED_TYPE: &str = "org.nspasteboard.ConcealedType";

/// The nspasteboard.org marker naming the bundle id of the app that wrote the
/// current contents, when the writer bothers to say.
pub const SOURCE_TYPE: &str = "org.nspasteboard.source";

/// What the system will let this process read, as of macOS 26.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Never prompted yet: the first content read may raise a system alert.
    Default,
    /// The user is asked on each programmatic content read.
    Ask,
    AlwaysAllow,
    /// Content reads return nothing, silently. A "no text" answer under this
    /// is not evidence that the clipboard is empty.
    AlwaysDeny,
    /// A value this build does not know.
    Other(isize),
    /// The OS predates the setting (before macOS 26): reads are unrestricted.
    NotApplicable,
}

impl Access {
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Default => "default (macOS may ask the user on the first read)",
            Access::Ask => "ask (macOS asks the user on each read)",
            Access::AlwaysAllow => "always allow",
            Access::AlwaysDeny => {
                "ALWAYS DENY (macOS hides the contents from this process; the clipboard may not be empty)"
            }
            Access::Other(_) => "unrecognized",
            Access::NotApplicable => "unrestricted (this macOS has no pasteboard privacy setting)",
        }
    }
}

/// The metadata of the current contents — never the contents themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peek {
    /// Bumped by the system on every write by anyone. Two equal counts mean
    /// nothing was written in between.
    pub change_count: isize,
    /// Every type on the pasteboard, in the order AppKit reports them.
    pub types: Vec<String>,
}

fn general() -> objc2::rc::Retained<NSPasteboard> {
    NSPasteboard::generalPasteboard()
}

/// The change count and type list, without reading any contents.
pub fn peek() -> Peek {
    autoreleasepool(|_| {
        let pb = general();
        let types = pb
            .types()
            .map(|a| a.iter().map(|t| t.to_string()).collect())
            .unwrap_or_default();
        Peek {
            change_count: pb.changeCount(),
            types,
        }
    })
}

/// The current change count alone.
pub fn change_count() -> isize {
    autoreleasepool(|_| general().changeCount())
}

/// What macOS will let this process read.
pub fn access() -> Access {
    autoreleasepool(|_| {
        let pb = general();
        // `accessBehavior` is new in macOS 26. Sending it to an older AppKit
        // would raise an unrecognized-selector exception, which must not cross
        // the FFI boundary, so ask first.
        if !pb.respondsToSelector(objc2::sel!(accessBehavior)) {
            return Access::NotApplicable;
        }
        let b = pb.accessBehavior();
        match b {
            NSPasteboardAccessBehavior::Default => Access::Default,
            NSPasteboardAccessBehavior::Ask => Access::Ask,
            NSPasteboardAccessBehavior::AlwaysAllow => Access::AlwaysAllow,
            NSPasteboardAccessBehavior::AlwaysDeny => Access::AlwaysDeny,
            other => Access::Other(other.0),
        }
    })
}

/// The string stored under `uti`, if any. A content read.
fn string_for(uti: &str) -> Option<String> {
    autoreleasepool(|_| {
        let ty = NSString::from_str(uti);
        general().stringForType(&ty).map(|s| s.to_string())
    })
}

/// The bundle id the writer declared under [`SOURCE_TYPE`], if it declared one.
pub fn source() -> Option<String> {
    string_for(SOURCE_TYPE)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The plain-text contents, if there are any.
pub fn text() -> Option<String> {
    autoreleasepool(|_| {
        // SAFETY: `NSPasteboardTypeString` is an immutable AppKit constant
        // that lives for the process.
        let ty = unsafe { NSPasteboardTypeString };
        general().stringForType(ty).map(|s| s.to_string())
    })
}

/// Replace the contents with one plain-text item.
///
/// Returns the change count after the write. `clearContents` and the string
/// write are two calls, so another writer can land between them; the caller
/// compares the returned count against [`peek`] if it needs to know.
pub fn write_text(text: &str) -> Result<isize, String> {
    autoreleasepool(|_| {
        let pb = general();
        pb.clearContents();
        let s = NSString::from_str(text);
        // SAFETY: as in `text`.
        let ty = unsafe { NSPasteboardTypeString };
        if pb.setString_forType(&s, ty) {
            Ok(pb.changeCount())
        } else {
            Err("NSPasteboard refused the string write (setString:forType: returned NO)".into())
        }
    })
}

/// Empty the pasteboard. Returns the change count after.
pub fn clear() -> isize {
    autoreleasepool(|_| general().clearContents())
}
