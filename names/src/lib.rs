//! File names (FS-SPEC F6, TIDY-UP T146). **One rule, in one place**: the servers, the pure file-system crates, the client library
//! and the libc all validate through this crate, so that no layer can disagree about what a name is.
//!
//! A name is a directory entry, one component of a path, or (with [`validate_text`]) an attribute name:
//! * valid UTF-8 (no overlong forms, no surrogates, nothing above U+10FFFF);
//! * no control characters: C0 (U+0000 to U+001F, so no NUL, which is reported as its own fault), DEL (U+007F) and C1 (U+0080 to U+009F), which makes a name always
//!   printable text and safe in line-based output;
//! * no `/`;
//! * not empty, not `.` and not `..`;
//! * at most [`MAX_NAME`] bytes.
//!
//! Names are compared byte for byte and case-sensitively and are stored as given (no normalisation; T141 records the direction
//! for later). A name that breaks the rule is **refused**, never repaired.
#![no_std]
#![forbid(unsafe_code)]

/// The longest name, in bytes.
pub const MAX_NAME: usize = 255;

/// Why a name was refused. The order of the checks is the order of the variants, so the answer for a name with several faults is the
/// first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    /// A NUL (a C string cannot hold one: `EINVAL`, not `EILSEQ`).
    Nul,
    /// A `/`.
    Slash,
    /// `.` or `..`.
    Dots,
    TooLong,
    /// Not valid UTF-8.
    NotUtf8,
    /// A control character other than NUL (C0, DEL or C1).
    Control,
}

/// Is `c` a control character: C0, DEL or C1 (Unicode general category Cc)?
fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
}

/// The text rule alone: valid UTF-8 without control characters (NUL is reported as `Control` here). Used for attribute names (which may contain `.`) and by
/// [`validate`].
pub fn validate_text(n: &[u8]) -> Result<(), NameError> {
    let s = core::str::from_utf8(n).map_err(|_| NameError::NotUtf8)?;
    if s.chars().any(is_control) {
        return Err(NameError::Control);
    }
    Ok(())
}

/// Validate a directory-entry name or one path component.
pub fn validate(n: &[u8]) -> Result<(), NameError> {
    if n.is_empty() {
        return Err(NameError::Empty);
    }
    if n.contains(&0) {
        return Err(NameError::Nul);
    }
    if n.contains(&b'/') {
        return Err(NameError::Slash);
    }
    if n == b"." || n == b".." {
        return Err(NameError::Dots);
    }
    if n.len() > MAX_NAME {
        return Err(NameError::TooLong);
    }
    validate_text(n)
}
