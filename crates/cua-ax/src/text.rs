// ── text types ───────────────────────────────────────────────────────────────

/// A character range inside an element's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextRange {
    pub offset: usize,
    pub length: usize,
}

/// Which mechanism [`Element::append_text`] ended up using.
///
/// Surfaced rather than hidden because the two are not equivalent from the app's
/// point of view, and a caller debugging "my text went in but the app did not
/// notice" needs to know which one happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextWrite {
    /// Written through `AXSelectedText` at a collapsed caret. Existing contents
    /// preserved.
    Inserted,
    /// Whole `AXValue` replaced with old + new.
    Replaced,
}

impl TextWrite {
    pub fn as_str(self) -> &'static str {
        match self {
            TextWrite::Inserted => "inserted",
            TextWrite::Replaced => "replaced",
        }
    }
}

/// Locate `needle` in `haystack`, optionally anchored by `prefix`/`suffix`.
///
/// Returns a **char**-based range covering only `needle`. Pure and total: no AX
/// involved, which is what makes the disambiguation logic testable.
pub fn find_text_range(
    haystack: &str,
    needle: &str,
    prefix: Option<&str>,
    suffix: Option<&str>,
) -> Option<TextRange> {
    if needle.is_empty() {
        return None;
    }
    let pre = prefix.unwrap_or("");
    let suf = suffix.unwrap_or("");
    let pattern = format!("{pre}{needle}{suf}");

    let byte_at = haystack.find(&pattern)?;
    // Skip past the prefix so the returned range covers the needle alone.
    let needle_byte = byte_at + pre.len();

    Some(TextRange {
        offset: haystack[..needle_byte].chars().count(),
        length: needle.chars().count(),
    })
}

/// Convert a char offset to a UTF-16 code-unit offset.
///
/// AX text ranges are counted in UTF-16 units because the API predates any
/// notion of scalar-based indexing. For ASCII the two agree, which is exactly
/// why this bug survives testing: it only appears once the text contains CJK,
/// emoji, or anything else outside the BMP. An emoji is one `char` and two UTF-16
/// units, so a selection past one is off by one per emoji.
pub fn utf16_offset(s: &str, char_offset: usize) -> usize {
    s.chars().take(char_offset).map(char::len_utf16).sum()
}
