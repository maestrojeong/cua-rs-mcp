//! `Inner` methods for the clipboard.
//!
//! Run on the native worker like every other native call. They need nothing of
//! `Inner` but the yield watch — no snapshot, no app — which is the point: the
//! clipboard is session state, and these never resolve or address an app.

use super::*;
use crate::clipboard;

/// What `clipboard_get` found in the text slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContents {
    /// Plain text was on the clipboard.
    Text(String),
    /// No plain-text representation: an image, a file list, or nothing at all.
    /// Not an error — the type list says what is there instead.
    NoText,
    /// There may be text, and policy kept it back. The reason quotes the
    /// evidence. The text was never read into this process.
    Withheld(String),
}

/// One clipboard read.
#[derive(Debug, Clone)]
pub struct ClipboardRead {
    pub contents: ClipboardContents,
    /// Every type on the clipboard. Format names only, never contents, so they
    /// are returned in every branch, including a withheld one.
    pub types: Vec<String>,
    pub change_count: isize,
    /// What macOS 26 pasteboard privacy will let this process read. Under
    /// [`ClipboardAccess::AlwaysDeny`] a `NoText` is not evidence of anything.
    pub access: ClipboardAccess,
}

/// One clipboard write or clear.
#[derive(Debug, Clone)]
pub struct ClipboardWrite {
    pub verb: &'static str,
    /// The change count the write produced.
    pub change_count: isize,
    /// Characters written; `0` for a clear.
    pub chars: usize,
    /// Whether a metadata re-read agrees with what was written: same change
    /// count (nobody wrote in between), and the type list is plain text for a
    /// set or empty for a clear. Checked without reading the contents back,
    /// because that read is the one macOS 26 may raise an alert for.
    pub verified: bool,
}

pub use clipboard::Access as ClipboardAccess;

impl Inner {
    pub(super) fn clipboard_get(&self) -> Result<ClipboardRead> {
        let before = clipboard::peek();
        let access = clipboard::access();
        let allow = crate::safety::forbidden_targets_allowed();
        let frontmost = apps::frontmost_pid()
            .and_then(|pid| apps::list_apps().into_iter().find(|a| a.pid == pid));

        let withheld = |reason: String| ClipboardRead {
            contents: ClipboardContents::Withheld(reason),
            types: before.types.clone(),
            change_count: before.change_count,
            access,
        };

        // Metadata first. Everything decidable without reading contents is
        // decided before any content read happens, so a withheld secret never
        // enters this process at all.
        if let Some(reason) =
            crate::safety::clipboard_read_refusal(&before.types, None, frontmost.as_ref(), allow)
        {
            return Ok(withheld(reason));
        }
        if before.types.iter().any(|t| t == clipboard::SOURCE_TYPE) {
            let source = clipboard::source();
            if let Some(reason) = crate::safety::clipboard_read_refusal(
                &before.types,
                source.as_deref(),
                frontmost.as_ref(),
                allow,
            ) {
                return Ok(withheld(reason));
            }
        }

        let text = clipboard::text();
        // The checks above judged the contents as of `before`. If anybody wrote
        // since, the text just read is something they never judged — possibly a
        // password copied in the gap — so it is dropped rather than returned.
        if clipboard::change_count() != before.change_count {
            return Err(CoreError::Clipboard {
                verb: "clipboard_get",
                reason: "the clipboard changed while it was being read, so the text was \
                         discarded rather than returned unchecked. Retry"
                    .into(),
            });
        }
        Ok(ClipboardRead {
            contents: match text {
                Some(t) => ClipboardContents::Text(t),
                None => ClipboardContents::NoText,
            },
            types: before.types,
            change_count: before.change_count,
            access,
        })
    }

    pub(super) fn clipboard_set(&self, text: &str) -> Result<ClipboardWrite> {
        const VERB: &str = "clipboard_set";
        crate::safety::guard_session(VERB, &self.human)?;
        let change_count = clipboard::write_text(text)
            .map_err(|reason| CoreError::Clipboard { verb: VERB, reason })?;
        let after = clipboard::peek();
        let verified = after.change_count == change_count
            && after.types.iter().any(|t| t == clipboard::STRING_TYPE);
        Ok(ClipboardWrite {
            verb: VERB,
            change_count,
            chars: text.chars().count(),
            verified,
        })
    }

    pub(super) fn clipboard_clear(&self) -> Result<ClipboardWrite> {
        const VERB: &str = "clipboard_clear";
        crate::safety::guard_session(VERB, &self.human)?;
        let change_count = clipboard::clear();
        let after = clipboard::peek();
        Ok(ClipboardWrite {
            verb: VERB,
            change_count,
            chars: 0,
            verified: after.change_count == change_count && after.types.is_empty(),
        })
    }
}
