//! `<search.h>` (`insque`, `remque`, `lsearch`, `lfind`, the `hsearch` table, the `tsearch` tree), `readv` and `writev`
//! (`<sys/uio.h>`), and `nl_langinfo` (`<langinfo.h>`): the next batch of the gap list (`LIBC-GAPS.md`, phase 2).

use crate::{errno, fd};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::ffi::{c_char, c_int, c_void, CStr};
use rt::sync::Mutex;

const ENOENT: c_int = 2;
const ENOMEM: c_int = 12;
const EINVAL: c_int = 22;

// ---- insque and remque: a doubly linked list whose elements start with next and prev pointers ----

#[repr(C)]
pub struct QElem {
    next: *mut QElem,
    prev: *mut QElem,
}

#[no_mangle]
pub unsafe extern "C" fn insque(elem: *mut c_void, pred: *mut c_void) {
    let e = elem as *mut QElem;
    let p = pred as *mut QElem;
    if p.is_null() {
        (*e).next = core::ptr::null_mut();
        (*e).prev = core::ptr::null_mut();
        return;
    }
    (*e).next = (*p).next;
    (*e).prev = p;
    if !(*p).next.is_null() {
        (*(*p).next).prev = e;
    }
    (*p).next = e;
}

#[no_mangle]
pub unsafe extern "C" fn remque(elem: *mut c_void) {
    let e = elem as *mut QElem;
    if !(*e).next.is_null() {
        (*(*e).next).prev = (*e).prev;
    }
    if !(*e).prev.is_null() {
        (*(*e).prev).next = (*e).next;
    }
}

// ---- lsearch and lfind: a linear search of an array ----

type Cmp = unsafe extern "C" fn(*const c_void, *const c_void) -> c_int;

#[no_mangle]
pub unsafe extern "C" fn lfind(
    key: *const c_void,
    base: *const c_void,
    nelp: *mut usize,
    width: usize,
    cmp: Cmp,
) -> *mut c_void {
    for i in 0..*nelp {
        let e = (base as *const u8).add(i * width) as *const c_void;
        if cmp(key, e) == 0 {
            return e as *mut c_void;
        }
    }
    core::ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "C" fn lsearch(
    key: *const c_void,
    base: *mut c_void,
    nelp: *mut usize,
    width: usize,
    cmp: Cmp,
) -> *mut c_void {
    let found = lfind(key, base, nelp, width, cmp);
    if !found.is_null() {
        return found;
    }
    let slot = (base as *mut u8).add(*nelp * width);
    core::ptr::copy_nonoverlapping(key as *const u8, slot, width);
    *nelp += 1;
    slot as *mut c_void
}

// ---- hsearch: one table of string keys, no deletion ----

#[repr(C)]
pub struct Entry {
    key: *mut c_char,
    data: *mut c_void,
}

struct HTable {
    slots: Vec<Entry>,
    used: Vec<bool>,
    count: usize,
    limit: usize,
}

// SAFETY: the table is only reached through the mutex, and the pointers in it belong to the caller.
unsafe impl Send for HTable {}

static HTAB: Mutex<Option<HTable>> = Mutex::new(None);

fn hash(k: &[u8]) -> usize {
    let mut h: usize = 5381;
    for &b in k {
        h = h.wrapping_mul(33).wrapping_add(usize::from(b));
    }
    h
}

#[no_mangle]
pub extern "C" fn hcreate(nel: usize) -> c_int {
    let mut g = HTAB.lock();
    if g.is_some() {
        return 1; // (glibc: a second hcreate leaves the table as it is)
    }
    // some room beyond `nel` so that probing stays short; the table still holds `nel` entries
    let size = (nel.max(1) * 4 / 3 + 3) | 1;
    let mut slots = Vec::new();
    if slots.try_reserve_exact(size).is_err() {
        errno::set(ENOMEM);
        return 0;
    }
    for _ in 0..size {
        slots.push(Entry {
            key: core::ptr::null_mut(),
            data: core::ptr::null_mut(),
        });
    }
    *g = Some(HTable {
        slots,
        used: alloc::vec![false; size],
        count: 0,
        limit: nel.max(1),
    });
    1
}

#[no_mangle]
pub extern "C" fn hdestroy() {
    *HTAB.lock() = None;
}

/// `hsearch(item, action)`: `ENTER` is 1, `FIND` is 0.
#[no_mangle]
pub unsafe extern "C" fn hsearch(item: Entry, action: c_int) -> *mut Entry {
    let mut g = HTAB.lock();
    let Some(t) = g.as_mut() else {
        errno::set(ESRCH_NO_TABLE);
        return core::ptr::null_mut();
    };
    let key = CStr::from_ptr(item.key).to_bytes();
    let n = t.slots.len();
    let mut i = hash(key) % n;
    for _ in 0..n {
        if !t.used[i] {
            if action == 0 {
                errno::set(ENOENT);
                return core::ptr::null_mut();
            }
            if t.count >= t.limit {
                errno::set(ENOMEM);
                return core::ptr::null_mut();
            }
            t.used[i] = true;
            t.count += 1;
            t.slots[i] = item;
            return &mut t.slots[i];
        }
        if CStr::from_ptr(t.slots[i].key).to_bytes() == key {
            return &mut t.slots[i];
        }
        i = (i + 1) % n;
    }
    errno::set(if action == 0 { ENOENT } else { ENOMEM });
    core::ptr::null_mut()
}

/// glibc sets `ESRCH` when `hsearch` is used with no table.
const ESRCH_NO_TABLE: c_int = 3;

// ---- tsearch: a binary search tree (unbalanced; `twalk` visits in the order glibc's tree would for `endorder`) ----

struct Node {
    key: *const c_void,
    left: *mut Node,
    right: *mut Node,
}

#[no_mangle]
pub unsafe extern "C" fn tsearch(
    key: *const c_void,
    rootp: *mut *mut c_void,
    cmp: Cmp,
) -> *mut c_void {
    if rootp.is_null() {
        return core::ptr::null_mut();
    }
    let mut link = rootp as *mut *mut Node;
    while !(*link).is_null() {
        let n = *link;
        match cmp(key, (*n).key).cmp(&0) {
            Ordering::Equal => return n as *mut c_void,
            Ordering::Less => link = &mut (*n).left,
            Ordering::Greater => link = &mut (*n).right,
        }
    }
    let n = alloc::boxed::Box::into_raw(alloc::boxed::Box::new(Node {
        key,
        left: core::ptr::null_mut(),
        right: core::ptr::null_mut(),
    }));
    *link = n;
    n as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn tfind(
    key: *const c_void,
    rootp: *const *mut c_void,
    cmp: Cmp,
) -> *mut c_void {
    if rootp.is_null() {
        return core::ptr::null_mut();
    }
    let mut n = *rootp as *mut Node;
    while !n.is_null() {
        match cmp(key, (*n).key).cmp(&0) {
            Ordering::Equal => return n as *mut c_void,
            Ordering::Less => n = (*n).left,
            Ordering::Greater => n = (*n).right,
        }
    }
    core::ptr::null_mut()
}

/// Returns the parent of the deleted node (or the new root's node when the root went), as POSIX says; NULL when absent.
#[no_mangle]
pub unsafe extern "C" fn tdelete(
    key: *const c_void,
    rootp: *mut *mut c_void,
    cmp: Cmp,
) -> *mut c_void {
    if rootp.is_null() {
        return core::ptr::null_mut();
    }
    let mut link = rootp as *mut *mut Node;
    let mut parent: *mut Node = core::ptr::null_mut();
    while !(*link).is_null() {
        let n = *link;
        match cmp(key, (*n).key).cmp(&0) {
            Ordering::Less => {
                parent = n;
                link = &mut (*n).left;
            }
            Ordering::Greater => {
                parent = n;
                link = &mut (*n).right;
            }
            Ordering::Equal => {
                let dead = n;
                let replacement = if (*n).left.is_null() {
                    (*n).right
                } else if (*n).right.is_null() {
                    (*n).left
                } else {
                    // the in-order successor takes its place
                    let mut sl: *mut *mut Node = &mut (*n).right;
                    while !(**sl).left.is_null() {
                        sl = &mut (**sl).left;
                    }
                    let succ = *sl;
                    *sl = (*succ).right;
                    (*succ).left = (*n).left;
                    (*succ).right = (*n).right;
                    succ
                };
                *link = replacement;
                drop(alloc::boxed::Box::from_raw(dead));
                return if parent.is_null() {
                    replacement as *mut c_void
                } else {
                    parent as *mut c_void
                };
            }
        }
    }
    core::ptr::null_mut()
}

/// `VISIT`: preorder 0, postorder 1, endorder 2, leaf 3.
type Action = unsafe extern "C" fn(*const c_void, c_int, c_int);

unsafe fn walk(n: *const Node, f: Action, depth: c_int) {
    if n.is_null() {
        return;
    }
    if (*n).left.is_null() && (*n).right.is_null() {
        f(n as *const c_void, 3, depth);
        return;
    }
    f(n as *const c_void, 0, depth);
    walk((*n).left, f, depth + 1);
    f(n as *const c_void, 1, depth);
    walk((*n).right, f, depth + 1);
    f(n as *const c_void, 2, depth);
}

#[no_mangle]
pub unsafe extern "C" fn twalk(root: *const c_void, f: Action) {
    walk(root as *const Node, f, 0);
}

unsafe fn destroy(n: *mut Node, free_node: unsafe extern "C" fn(*mut c_void)) {
    if n.is_null() {
        return;
    }
    destroy((*n).left, free_node);
    destroy((*n).right, free_node);
    free_node((*n).key as *mut c_void);
    drop(alloc::boxed::Box::from_raw(n));
}

#[no_mangle]
pub unsafe extern "C" fn tdestroy(root: *mut c_void, free_node: unsafe extern "C" fn(*mut c_void)) {
    destroy(root as *mut Node, free_node);
}

// ---- readv and writev ----

#[repr(C)]
pub struct IoVec {
    base: *mut c_void,
    len: usize,
}

#[no_mangle]
pub unsafe extern "C" fn writev(fd: c_int, iov: *const IoVec, n: c_int) -> isize {
    if !(0..=1024).contains(&n) || (n > 0 && iov.is_null()) {
        errno::set(EINVAL);
        return -1;
    }
    let mut total: isize = 0;
    for i in 0..n as usize {
        let v = &*iov.add(i);
        if v.len == 0 {
            continue;
        }
        let w = fd::write(fd, v.base, v.len);
        if w < 0 {
            return if total > 0 { total } else { -1 };
        }
        total += w;
        if (w as usize) < v.len {
            break; // a short write: report what went
        }
    }
    total
}

#[no_mangle]
pub unsafe extern "C" fn readv(fd: c_int, iov: *const IoVec, n: c_int) -> isize {
    if !(0..=1024).contains(&n) || (n > 0 && iov.is_null()) {
        errno::set(EINVAL);
        return -1;
    }
    let mut total: isize = 0;
    for i in 0..n as usize {
        let v = &*iov.add(i);
        if v.len == 0 {
            continue;
        }
        let r = fd::read(fd, v.base, v.len);
        if r < 0 {
            return if total > 0 { total } else { -1 };
        }
        total += r;
        if (r as usize) < v.len {
            break; // end of input or a short read
        }
    }
    total
}

// ---- nl_langinfo: the C locale, whose one encoding is UTF-8 ----

#[no_mangle]
pub extern "C" fn nl_langinfo(item: c_int) -> *const c_char {
    let s: &'static [u8] = match item {
        14 => b"UTF-8\0",      // CODESET
        0x10000 => b".\0",     // RADIXCHAR
        0x10001 => b"\0",      // THOUSEP
        0x50000 => b"^[yY]\0", // YESEXPR
        0x50001 => b"^[nN]\0", // NOEXPR
        _ => b"\0",
    };
    s.as_ptr() as *const c_char
}
