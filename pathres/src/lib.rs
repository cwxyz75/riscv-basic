//! Path resolution for a process whose view is its **namespace table** (USER-VIEW-SPEC section 5).
//!
//! * The protocol forbids `..`; the sandbox is the handles. The libc therefore resolves `..` **lexically**, against the
//!   process's own table, and **clamps at the table's root**: `/..` is `/`.
//! * The **cwd is a path string** (absolute components) plus its handle. A relative path that never climbs above the cwd
//!   starts from the cwd handle; one that climbs is re-walked from the table prefix that covers the resulting absolute path.
//!   A process with no `/` entry therefore cannot climb out of its cwd: the walk finds no prefix and fails.
//! * **Symlinks** are followed here, not by servers: a server that meets a link while walking a path stops and says how many
//!   components it consumed and what the target text is ([`Stop::Symlink`]); the caller resolves the target lexically from the
//!   link's own directory and starts again, up to [`MAX_LINKS`] times (`ELOOP`).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;

/// Links followed in one resolution before giving up (`ELOOP`).
pub const MAX_LINKS: usize = 40;

pub type Comps = Vec<Vec<u8>>;

/// Split `path` into components, dropping empty ones and `.`; `..` is kept (it is [`clamp`]'s business).
fn split(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
}

/// Resolve `..` lexically in `comps` pushed onto `base` (absolute components): a `..` at the root stays at the root.
pub fn clamp(base: &[Vec<u8>], path: &[u8]) -> Comps {
    let mut out: Comps = base.to_vec();
    for c in split(path) {
        if c == b".." {
            out.pop();
        } else {
            out.push(c.to_vec());
        }
    }
    out
}

/// The absolute components of `path` for a process whose cwd is `cwd` (absolute components), clamped at the root.
pub fn absolute(cwd: &[Vec<u8>], path: &[u8]) -> Comps {
    if path.first() == Some(&b'/') {
        clamp(&[], path)
    } else {
        clamp(cwd, path)
    }
}

/// Components as a path: `/a/b`, or `/` for none.
pub fn join_abs(comps: &[Vec<u8>]) -> Vec<u8> {
    if comps.is_empty() {
        return alloc::vec![b'/'];
    }
    let mut out = Vec::new();
    for c in comps {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    out
}

/// Components as a relative path (`a/b`; empty for none).
pub fn join_rel(comps: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(c);
    }
    out
}

/// The components of an absolute path (`/a/b`), clamped.
pub fn parse_abs(path: &[u8]) -> Comps {
    clamp(&[], path)
}

/// The process's namespace table: absolute prefixes to handles, and the cwd.
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// (prefix components, handle): `/` is the empty prefix. Any absolute prefix (`/mnt/store`, `/svc`) may appear.
    pub mounts: Vec<(Comps, u32)>,
    /// The cwd: its absolute components (if known) and its handle (if the process has one).
    pub cwd: Option<Comps>,
    pub cwd_handle: Option<u32>,
}

/// Where to open something: the handle and the path relative to it (components).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub handle: u32,
    pub rel: Comps,
    /// The absolute components of the directory `handle` stands for (the prefix, or the cwd): what a link found below it is
    /// resolved against.
    pub base: Comps,
}

impl Table {
    /// The mount whose prefix is the longest one covering `abs`.
    pub fn locate(&self, abs: &[Vec<u8>]) -> Option<Place> {
        let mut best: Option<(&Comps, u32)> = None;
        for (p, h) in &self.mounts {
            if abs.len() >= p.len()
                && abs[..p.len()] == p[..]
                && best.is_none_or(|(b, _)| p.len() > b.len())
            {
                best = Some((p, *h));
            }
        }
        best.map(|(p, h)| Place {
            handle: h,
            rel: abs[p.len()..].to_vec(),
            base: p.clone(),
        })
    }

    /// Where `path` is opened. A relative path that never climbs above the cwd starts at the cwd handle (the fast case);
    /// anything else is made absolute and re-walked from the covering mount. `None`: nothing covers it.
    pub fn place(&self, path: &[u8]) -> Option<Place> {
        if path.first() != Some(&b'/') {
            if let Some(h) = self.cwd_handle {
                // does the relative path stay below the cwd? (depth never negative)
                let mut depth = 0i64;
                let mut stays = true;
                for c in split(path) {
                    if c == b".." {
                        depth -= 1;
                        if depth < 0 {
                            stays = false;
                            break;
                        }
                    } else {
                        depth += 1;
                    }
                }
                if stays {
                    let rel = clamp(&[], path);
                    return Some(Place {
                        handle: h,
                        rel,
                        base: self.cwd.clone().unwrap_or_default(),
                    });
                }
            }
        }
        let cwd = self.cwd.clone().unwrap_or_default();
        if path.first() != Some(&b'/') && self.cwd.is_none() {
            return None; // no cwd string: a path that climbs cannot be placed
        }
        self.locate(&absolute(&cwd, path))
    }
}

/// Why a server stopped a walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It met a symlink after consuming `consumed` components of the relative path (the link included); `target` is the text.
    Symlink { consumed: usize, target: Vec<u8> },
    /// Any other failure: the protocol status.
    Status(i32),
}

/// Why a resolution failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fail {
    /// The server's status, or `NOT_FOUND`-like for a path nothing covers.
    Status(i32),
    /// Nothing in the table covers the path.
    NoMount,
    /// More than [`MAX_LINKS`] links.
    Loop,
}

/// Resolve and open `path`, following links. `open` is called with a handle and a relative path and either succeeds or says
/// what stopped it. Returns what `open` returned, with the absolute components that were finally opened.
pub fn open_following<T>(
    table: &Table,
    path: &[u8],
    mut open: impl FnMut(u32, &[u8]) -> Result<T, Stop>,
) -> Result<(T, Comps), Fail> {
    // POSIX: the empty path names nothing (`open("")` is ENOENT, not the cwd)
    if path.is_empty() {
        return Err(Fail::NoMount);
    }
    let mut place = table.place(path).ok_or(Fail::NoMount)?;
    for _ in 0..=MAX_LINKS {
        let rel = join_rel(&place.rel);
        match open(place.handle, &rel) {
            Ok(t) => {
                let mut abs = place.base.clone();
                abs.extend(place.rel.iter().cloned());
                return Ok((t, abs));
            }
            Err(Stop::Status(s)) => return Err(Fail::Status(s)),
            Err(Stop::Symlink { consumed, target }) => {
                if consumed == 0 || consumed > place.rel.len() {
                    return Err(Fail::Status(-1)); // a server that makes no sense
                }
                // the directory the link is in, and what is left of the path after it
                let mut dir = place.base.clone();
                dir.extend(place.rel[..consumed - 1].iter().cloned());
                let mut next = absolute(&dir, &target);
                next.extend(place.rel[consumed..].iter().cloned());
                place = table.locate(&next).ok_or(Fail::NoMount)?;
            }
        }
    }
    Err(Fail::Loop)
}
