//! The mount table (FS-SPEC section 6). A volume is **identified by its uuid**; this table maps a uuid to the local name it is
//! published under (`/svc/vol/<name>`); a disk can never claim a name by what it calls itself. The table says nothing about
//! who may use a volume (that is the grant list, `/etc/trusted` today).
//!
//! Format: one entry per line, `uuid name [options...]`; blank lines and everything after `#` are ignored; fields are split on
//! spaces and tabs. The uuid is 32 hex digits, dashes allowed anywhere between them. A name is 1 to 32 bytes of `a-z A-Z 0-9 . _ -`
//! not starting with `.`. Options: `ro` (publish read-only), `sub=<dir>` (publish this subdirectory of the volume instead of its
//! root; the path is relative, no `..`).
//!
//! [`plan`] is the other half: given the volumes `volmgr` found (a uuid and a label each) and the table, what is published and
//! under which names, with duplicates refused.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub uuid: [u8; 16],
    pub name: String,
    pub readonly: bool,
    pub sub: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Fewer than two fields.
    MissingField,
    /// The uuid is not 32 hex digits.
    BadUuid,
    /// The name is empty, too long, or has a character outside the set.
    BadName,
    /// An option that is not defined, or a bad `sub=` path.
    BadOption,
    /// The same uuid or the same name twice.
    Duplicate,
    /// The text is not UTF-8.
    NotText,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number (0 when the whole text is bad).
    pub line: usize,
    pub kind: ErrorKind,
}

pub const MAX_NAME: usize = 32;
pub const MAX_ENTRIES: usize = 64;

/// Parse a uuid: 32 hex digits, dashes ignored.
pub fn parse_uuid(s: &str) -> Option<[u8; 16]> {
    let mut out = [0u8; 16];
    let mut n = 0;
    let mut hi: Option<u8> = None;
    for c in s.chars() {
        if c == '-' {
            continue;
        }
        let d = c.to_digit(16)? as u8;
        match hi.take() {
            None => hi = Some(d),
            Some(h) => {
                if n >= 16 {
                    return None;
                }
                out[n] = (h << 4) | d;
                n += 1;
            }
        }
    }
    (n == 16 && hi.is_none()).then_some(out)
}

/// The uuid as 32 lower-case hex digits (the name under `/svc/vol/by-uuid/`).
pub fn uuid_hex(u: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in u {
        for d in [b >> 4, b & 15] {
            s.push(char::from_digit(u32::from(d), 16).unwrap_or('0'));
        }
    }
    s
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && !n.starts_with('.')
        && n.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn valid_sub(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 255
        && !p.starts_with('/')
        && p.split('/').all(|c| {
            !c.is_empty() && c != "." && c != ".." && !c.bytes().any(|b| b == 0 || b < 0x20)
        })
}

/// Parse the table.
pub fn parse(text: &[u8]) -> Result<Vec<Entry>, ParseError> {
    let text = core::str::from_utf8(text).map_err(|_| ParseError {
        line: 0,
        kind: ErrorKind::NotText,
    })?;
    let mut out: Vec<Entry> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let err = |kind| ParseError { line: i + 1, kind };
        let line = raw.split('#').next().unwrap_or("");
        let mut f = line.split([' ', '\t']).filter(|s| !s.is_empty());
        let Some(u) = f.next() else { continue };
        let name = f.next().ok_or(err(ErrorKind::MissingField))?;
        let uuid = parse_uuid(u).ok_or(err(ErrorKind::BadUuid))?;
        if !valid_name(name) {
            return Err(err(ErrorKind::BadName));
        }
        let (mut readonly, mut sub) = (false, None);
        for o in f {
            if o == "ro" {
                readonly = true;
            } else if let Some(p) = o.strip_prefix("sub=") {
                if !valid_sub(p) || sub.is_some() {
                    return Err(err(ErrorKind::BadOption));
                }
                sub = Some(String::from(p));
            } else {
                return Err(err(ErrorKind::BadOption));
            }
        }
        if out.iter().any(|e| e.uuid == uuid || e.name == name) || out.len() >= MAX_ENTRIES {
            return Err(err(ErrorKind::Duplicate));
        }
        out.push(Entry {
            uuid,
            name: String::from(name),
            readonly,
            sub,
        });
    }
    Ok(out)
}

/// A volume `volmgr` recognised on some device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The device number (in `devmgr`'s order).
    pub device: u16,
    pub uuid: [u8; 16],
    /// Display text from the superblock; never used for naming.
    pub label: String,
}

/// What to do with one found volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Serve it: always under `by-uuid/<hex>`, and under `name` too if the table maps its uuid.
    Publish {
        by_uuid: String,
        name: Option<String>,
        readonly: bool,
        sub: Option<String>,
    },
    /// A second device with the same uuid as an earlier one (a cloned disk): not served, the first keeps its names.
    RefuseDuplicate,
}

/// Decide for every found volume, in order. A uuid seen before is refused; names come only from the table.
pub fn plan(table: &[Entry], found: &[Found]) -> Vec<(Found, Decision)> {
    let mut seen: Vec<[u8; 16]> = Vec::new();
    let mut out = Vec::new();
    for f in found {
        if seen.contains(&f.uuid) {
            out.push((f.clone(), Decision::RefuseDuplicate));
            continue;
        }
        seen.push(f.uuid);
        let e = table.iter().find(|e| e.uuid == f.uuid);
        out.push((
            f.clone(),
            Decision::Publish {
                by_uuid: uuid_hex(&f.uuid),
                name: e.map(|e| e.name.clone()),
                readonly: e.is_some_and(|e| e.readonly),
                sub: e.and_then(|e| e.sub.clone()),
            },
        ));
    }
    out
}
