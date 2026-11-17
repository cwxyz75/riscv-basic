//! `glob` and `globfree` (POSIX): the directory is read with `dir_entries` and each component with a pattern is matched by
//! `cfmt::glob::fnmatch` (the pure core, tested against the host's libc); a leading `.` is matched only by a pattern that names it.

use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void};

const GLOB_MARK: c_int = 0x02;
const GLOB_NOSORT: c_int = 0x04;
const GLOB_DOOFFS: c_int = 0x08;
const GLOB_NOCHECK: c_int = 0x10;
const GLOB_APPEND: c_int = 0x20;
const GLOB_NOESCAPE: c_int = 0x40;
const GLOB_NOMATCH: c_int = 3;
const FNM_PATHNAME: i32 = 1;
const FNM_NOESCAPE: i32 = 2;
const FNM_PERIOD: i32 = 4;

#[repr(C)]
pub struct Glob {
    gl_pathc: usize,
    gl_pathv: *mut *mut c_char,
    gl_offs: usize,
    gl_flags: c_int,
}

fn has_magic(comp: &[u8], noescape: bool) -> bool {
    let mut i = 0;
    while i < comp.len() {
        match comp[i] {
            b'*' | b'?' | b'[' => return true,
            b'\\' if !noescape => i += 1,
            _ => {}
        }
        i += 1;
    }
    false
}

/// A component with the backslashes taken out (what a pattern without magic stands for).
fn unescape(comp: &[u8], noescape: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < comp.len() {
        if comp[i] == b'\\' && !noescape && i + 1 < comp.len() {
            i += 1;
        }
        out.push(comp[i]);
        i += 1;
    }
    out
}

fn join(base: &[u8], name: &[u8]) -> Vec<u8> {
    let mut p = base.to_vec();
    if !p.is_empty() && p.last() != Some(&b'/') {
        p.push(b'/');
    }
    p.extend_from_slice(name);
    p
}

/// The directory holding `path`'s last component, and that component.
fn split_last(path: &[u8]) -> (Vec<u8>, Vec<u8>) {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => (b"/".to_vec(), path[1..].to_vec()),
        Some(i) => (path[..i].to_vec(), path[i + 1..].to_vec()),
        None => (b".".to_vec(), path.to_vec()),
    }
}

fn exists(path: &[u8]) -> bool {
    let (dir, name) = split_last(path);
    crate::fd::dir_entries(&dir).is_ok_and(|l| l.iter().any(|(n, _, _)| *n == name))
}

fn expand(base: Vec<u8>, comps: &[&[u8]], noescape: bool, out: &mut Vec<Vec<u8>>) {
    let Some((first, rest)) = comps.split_first() else {
        out.push(base);
        return;
    };
    if !has_magic(first, noescape) {
        let next = join(&base, &unescape(first, noescape));
        // (the path must exist for a match to count)
        if rest.is_empty() && !exists(&next) {
            return;
        }
        expand(next, rest, noescape, out);
        return;
    }
    let dir: &[u8] = if base.is_empty() { b"." } else { &base };
    let Ok(list) = crate::fd::dir_entries(dir) else {
        return;
    };
    let flags = FNM_PATHNAME | FNM_PERIOD | if noescape { FNM_NOESCAPE } else { 0 };
    for (name, is_dir, _) in list {
        if name == b"." || name == b".." || !cfmt::glob::fnmatch(first, &name, flags) {
            continue;
        }
        if !rest.is_empty() && !is_dir {
            continue;
        }
        expand(join(&base, &name), rest, noescape, out);
    }
}

#[no_mangle]
pub unsafe extern "C" fn glob(
    pattern: *const c_char,
    flags: c_int,
    _errfunc: *const c_void,
    g: *mut Glob,
) -> c_int {
    let pat = core::ffi::CStr::from_ptr(pattern).to_bytes();
    let noescape = flags & GLOB_NOESCAPE != 0;
    let absolute = pat.first() == Some(&b'/');
    let comps: Vec<&[u8]> = pat
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .collect();
    let trailing_slash = pat.len() > 1 && pat.ends_with(b"/");
    let mut found: Vec<Vec<u8>> = Vec::new();
    expand(
        if absolute { b"/".to_vec() } else { Vec::new() },
        &comps,
        noescape,
        &mut found,
    );
    if flags & GLOB_NOSORT == 0 {
        found.sort();
    }
    if flags & GLOB_MARK != 0 || trailing_slash {
        let mut keep = Vec::new();
        for mut p in found {
            let (dir, name) = split_last(&p);
            let is_dir = crate::fd::dir_entries(&dir)
                .ok()
                .is_some_and(|l| l.iter().any(|(n, d, _)| *n == name && *d));
            if trailing_slash && !is_dir {
                continue;
            }
            if is_dir {
                p.push(b'/');
            }
            keep.push(p);
        }
        found = keep;
    }
    if found.is_empty() {
        if flags & GLOB_NOCHECK != 0 {
            found.push(pat.to_vec());
        } else {
            return GLOB_NOMATCH;
        }
    }
    // the result array: `gl_offs` leading nulls when asked, what an earlier call found when appending
    let offs = if flags & GLOB_DOOFFS != 0 {
        (*g).gl_offs
    } else {
        0
    };
    let mut all: Vec<*mut c_char> = Vec::new();
    if flags & GLOB_APPEND != 0 && !(*g).gl_pathv.is_null() {
        all.extend_from_slice(core::slice::from_raw_parts(
            (*g).gl_pathv,
            offs + (*g).gl_pathc,
        ));
    } else {
        all.extend(core::iter::repeat_n(core::ptr::null_mut(), offs));
    }
    let old = (*g).gl_pathv;
    let old_len = offs + (*g).gl_pathc + 1;
    for mut p in found {
        p.push(0);
        // (boxed slices: freed by `globfree` with exactly the length they were made with)
        all.push(alloc::boxed::Box::into_raw(p.into_boxed_slice()) as *mut u8 as *mut c_char);
    }
    let count = all.len() - offs;
    all.push(core::ptr::null_mut());
    if flags & GLOB_APPEND != 0 && !old.is_null() {
        // the strings of the earlier call are in the new array; its array is not needed
        drop(alloc::boxed::Box::from_raw(
            core::ptr::slice_from_raw_parts_mut(old, old_len),
        ));
    }
    (*g).gl_pathv = alloc::boxed::Box::into_raw(all.into_boxed_slice()) as *mut *mut c_char;
    (*g).gl_pathc = count;
    (*g).gl_offs = offs;
    (*g).gl_flags = flags;
    0
}

#[no_mangle]
pub unsafe extern "C" fn globfree(g: *mut Glob) {
    if (*g).gl_pathv.is_null() {
        return;
    }
    let (offs, n) = ((*g).gl_offs, (*g).gl_pathc);
    let all = core::slice::from_raw_parts((*g).gl_pathv, offs + n + 1);
    for &p in &all[offs..offs + n] {
        if !p.is_null() {
            let len = core::ffi::CStr::from_ptr(p).to_bytes().len() + 1;
            drop(alloc::boxed::Box::from_raw(
                core::ptr::slice_from_raw_parts_mut(p as *mut u8, len),
            ));
        }
    }
    drop(alloc::boxed::Box::from_raw(
        core::ptr::slice_from_raw_parts_mut((*g).gl_pathv, offs + n + 1),
    ));
    (*g).gl_pathv = core::ptr::null_mut();
    (*g).gl_pathc = 0;
}
