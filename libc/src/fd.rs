//! File descriptors over the file protocol (S7c-3, OS-ARCHITECTURE section 10): `open`, `read`, `write`,
//! `lseek`, `close`, `unlink`, `mkdir`, `stat`, `isatty`. Paths resolve against the namespace the
//! program was started with: absolute paths against the `/` entry, relative ones against `.`; `..` is
//! refused by the server (a directory handle is a sandbox boundary). fds 0-2 are the standard streams
//! (sockets); a file fd is a connection to the file's server with an offset kept here.

use crate::errno;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_long, c_void};
use core::sync::atomic::{AtomicU32, Ordering};
use proto::file::{flags as pf, rights, status as ps};
use rt::fs::Fs;
use rt::sync::Mutex;

pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
pub const O_RDWR: c_int = 2;
pub const O_CREAT: c_int = 0x40;
pub const O_EXCL: c_int = 0x80;
pub const O_TRUNC: c_int = 0x200;
pub const O_APPEND: c_int = 0x400;

const MAX_FDS: usize = 64;
const CHUNK: usize = 16 * 1024;

pub struct OpenFile {
    fs: Fs,
    off: u64,
    append: bool,
    readable: bool,
    writable: bool,
}

enum Entry {
    /// A standard stream: the handle of a socket.
    Stream(u32),
    File(OpenFile),
}

struct Table {
    fds: [Option<Entry>; MAX_FDS],
}

static FDS: Mutex<Table> = Mutex::new(Table {
    fds: [const { None }; MAX_FDS],
});

/// The directory connections of the namespace (`/` and `.`), 0 if absent; owned by the startup message.
pub static ROOT: AtomicU32 = AtomicU32::new(0);
pub static CWD: AtomicU32 = AtomicU32::new(0);
/// Which standard streams are the terminal (`STDIN=tty`, `STDOUT=tty` in the environment).
pub static TTY_IN: AtomicU32 = AtomicU32::new(0);
pub static TTY_OUT: AtomicU32 = AtomicU32::new(0);

/// Install the three standard streams (handles 0 = absent).
pub fn init_std(stdin: u32, stdout: u32, stderr: u32) {
    let mut t = FDS.lock();
    for (i, h) in [stdin, stdout, stderr].into_iter().enumerate() {
        if h != 0 {
            t.fds[i] = Some(Entry::Stream(h));
        }
    }
}

fn errno_of(status: i32) -> c_int {
    match status {
        ps::NOT_FOUND => 2,      // ENOENT
        ps::EXISTS => 17,        // EEXIST
        ps::NOT_EMPTY => 39,     // ENOTEMPTY
        ps::IS_DIRECTORY => 21,  // EISDIR
        ps::NOT_DIRECTORY => 20, // ENOTDIR
        ps::NAME_TOO_LONG => 36, // ENAMETOOLONG
        ps::QUOTA => 28,         // ENOSPC
        ps::BAD_REQUEST => 22,   // EINVAL
        -3 => 13,                // EACCES
        -5 => 12,                // ENOMEM
        _ => 5,                  // EIO
    }
}

fn fail(status: i32) -> c_int {
    errno::set(errno_of(status));
    -1
}

/// A txid range per view of a shared directory connection (see `Fs::new_at`).
static TXIDS: AtomicU32 = AtomicU32::new(1);

fn dir_view(h: u32) -> Fs {
    Fs::new_at(h, TXIDS.fetch_add(1 << 12, Ordering::Relaxed))
}

/// The directory a path is relative to and the path to use there, cleaned up: empty components
/// (`a//b`, a trailing slash) and `.` are dropped; `..` is left for the server to refuse. `None` for
/// an empty path or a namespace without that directory.
fn resolve(path: &[u8]) -> Option<(u32, Vec<u8>)> {
    let h = if path.first() == Some(&b'/') {
        ROOT.load(Ordering::Relaxed)
    } else {
        CWD.load(Ordering::Relaxed)
    };
    let mut out: Vec<u8> = Vec::new();
    for comp in path
        .split(|&c| c == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
    {
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(comp);
    }
    (h != 0 && !out.is_empty()).then_some((h, out))
}

unsafe fn path_bytes<'a>(p: *const c_char) -> &'a [u8] {
    core::ffi::CStr::from_ptr(p).to_bytes()
}

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, _mode: c_int) -> c_int {
    let Some((dir, rel)) = resolve(path_bytes(path)) else {
        errno::set(2);
        return -1;
    };
    let access = flags & 3;
    let mut r = 0;
    if access == O_RDONLY || access == O_RDWR {
        r |= rights::READ;
    }
    if access == O_WRONLY || access == O_RDWR {
        r |= rights::WRITE;
    }
    if flags & O_APPEND != 0 {
        r |= rights::READ; // finding the end of the file (GETSIZE) needs it
    }
    let mut f = 0u16;
    if flags & O_CREAT != 0 {
        f |= pf::CREATE; // (the directory's own rights decide whether it may create)
    }
    if flags & O_EXCL != 0 {
        f |= pf::EXCLUSIVE;
    }
    if flags & O_TRUNC != 0 {
        f |= pf::TRUNCATE;
    }
    let mut t = FDS.lock();
    let Some(slot) = (3..MAX_FDS).find(|&i| t.fds[i].is_none()) else {
        errno::set(24); // EMFILE
        return -1;
    };
    let mut d = dir_view(dir); // a view of a connection the startup message owns: never closed here
    match d.open(&rel, f, r) {
        Ok(mut fs) => {
            // an append stream starts at the end (so `ftell` is right before the first write)
            let off = if flags & O_APPEND != 0 {
                fs.size().unwrap_or(0)
            } else {
                0
            };
            t.fds[slot] = Some(Entry::File(OpenFile {
                fs,
                off,
                append: flags & O_APPEND != 0,
                readable: access != O_WRONLY,
                writable: access != O_RDONLY,
            }));
            slot as c_int
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub extern "C" fn close(fd: c_int) -> c_int {
    let mut t = FDS.lock();
    match t.fds.get_mut(fd as usize).and_then(Option::take) {
        Some(Entry::File(f)) => {
            f.fs.close();
            0
        }
        Some(Entry::Stream(_)) => 0, // the handle belongs to the startup message
        None => {
            errno::set(9); // EBADF
            -1
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize {
    let mut t = FDS.lock();
    let Some(entry) = t.fds.get_mut(fd as usize).and_then(Option::as_mut) else {
        errno::set(9);
        return -1;
    };
    if n == 0 {
        return 0;
    }
    match entry {
        Entry::Stream(h) => {
            let h = *h;
            drop(t); // a blocking read must not hold the table
            read_stream(fd, h, buf as *mut u8, n)
        }
        Entry::File(f) => {
            if !f.readable {
                errno::set(9);
                return -1;
            }
            match f.fs.read(f.off, n.min(CHUNK)) {
                Ok(data) => {
                    core::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len());
                    f.off += data.len() as u64;
                    data.len() as isize
                }
                Err(e) => fail(e) as isize,
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, n: usize) -> isize {
    let data: &[u8] = if n == 0 {
        &[]
    } else {
        core::slice::from_raw_parts(buf as *const u8, n)
    };
    let mut t = FDS.lock();
    let Some(entry) = t.fds.get_mut(fd as usize).and_then(Option::as_mut) else {
        errno::set(9);
        return -1;
    };
    match entry {
        Entry::Stream(h) => {
            let h = *h;
            drop(t);
            if rt::io::write_all(h, data).is_ok() {
                n as isize
            } else {
                errno::set(32); // EPIPE
                -1
            }
        }
        Entry::File(f) => {
            if !f.writable {
                errno::set(9);
                return -1;
            }
            if f.append {
                match f.fs.size() {
                    Ok(s) => f.off = s,
                    Err(e) => return fail(e) as isize,
                }
            }
            let mut done = 0;
            while done < data.len() {
                let end = (done + CHUNK).min(data.len());
                match f.fs.write(f.off, &data[done..end]) {
                    Ok(0) => break,
                    Ok(k) => {
                        f.off += k as u64;
                        done += k;
                    }
                    Err(e) => {
                        if done > 0 {
                            break;
                        }
                        return fail(e) as isize;
                    }
                }
            }
            done as isize
        }
    }
}

pub const SEEK_SET: c_int = 0;
pub const SEEK_CUR: c_int = 1;
pub const SEEK_END: c_int = 2;

#[no_mangle]
pub extern "C" fn lseek(fd: c_int, off: c_long, whence: c_int) -> c_long {
    let mut t = FDS.lock();
    let Some(Entry::File(f)) = t.fds.get_mut(fd as usize).and_then(Option::as_mut) else {
        errno::set(if fd < 3 { 29 } else { 9 }); // ESPIPE / EBADF
        return -1;
    };
    let base = match whence {
        SEEK_SET => 0,
        SEEK_CUR => f.off as i64,
        SEEK_END => match f.fs.size() {
            Ok(s) => s as i64,
            Err(e) => return fail(e) as c_long,
        },
        _ => {
            errno::set(22);
            return -1;
        }
    };
    match base.checked_add(off).filter(|p| *p >= 0) {
        Some(p) => {
            f.off = p as u64;
            p as c_long
        }
        None => {
            errno::set(22);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn isatty(fd: c_int) -> c_int {
    let tty = match fd {
        0 => TTY_IN.load(Ordering::Relaxed),
        1 | 2 => TTY_OUT.load(Ordering::Relaxed),
        _ => 0,
    };
    if tty == 0 {
        errno::set(25); // ENOTTY
    }
    c_int::from(tty != 0)
}

/// Run `f` on the directory holding the last component of `path`, with that component. The parent
/// is opened with `parent_rights` only (reading and writing, not all four).
fn with_parent<R>(
    path: &[u8],
    parent_rights: u32,
    f: impl FnOnce(&mut Fs, &[u8]) -> Result<R, i32>,
) -> Result<R, i32> {
    let Some((dir, rel)) = resolve(path) else {
        return Err(ps::NOT_FOUND);
    };
    let mut d = dir_view(dir);
    match rel.iter().rposition(|&c| c == b'/') {
        None => f(&mut d, &rel),
        Some(i) => {
            let mut parent = d.open(&rel[..i], pf::DIRECTORY, parent_rights)?;
            let r = f(&mut parent, &rel[i + 1..]);
            parent.close();
            r
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    let _guard = FDS.lock();
    match with_parent(path_bytes(path), rights::READ | rights::WRITE, |d, name| {
        d.unlink(name)
    }) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rmdir(path: *const c_char) -> c_int {
    unlink(path)
}

#[no_mangle]
pub unsafe extern "C" fn mkdir(path: *const c_char, _mode: c_int) -> c_int {
    let _guard = FDS.lock();
    match with_parent(path_bytes(path), rights::READ | rights::WRITE, |d, name| {
        d.mkdir(name)
    }) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

/// `struct stat` here is not Linux's: the size and the kind are all the file protocol knows.
#[repr(C)]
pub struct Stat {
    pub st_size: i64,
    pub st_mode: u32,
}

#[no_mangle]
pub unsafe extern "C" fn stat(path: *const c_char, out: *mut Stat) -> c_int {
    let _guard = FDS.lock();
    // the server's STAT takes a whole path: no parent to open (and READ is all it needs)
    let r = match resolve(path_bytes(path)) {
        Some((dir, rel)) => dir_view(dir).stat(&rel),
        None => Err(ps::NOT_FOUND),
    };
    match r {
        Ok(s) => {
            (*out).st_size = s.size as i64;
            (*out).st_mode = if s.is_dir { 0o040_755 } else { 0o100_644 };
            0
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rename(_from: *const c_char, _to: *const c_char) -> c_int {
    errno::set(38); // ENOSYS: the file protocol has no rename (TIDY-UP T46)
    -1
}

/// A scratch name no other open file of this process has, in the working directory (`.`): the
/// program's writable scratch place is whatever directory it was given as `.`, there is no ambient
/// `/tmp` to reach (TIDY-UP T51).
pub fn scratch_name() -> Vec<u8> {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    alloc::format!("tmp{:x}-{n}", rt::sys::clock_get()).into_bytes()
}

/// Bytes read from the terminal that have not been handed out yet (standard input only).
static PENDING: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Read from a standard stream. On a terminal the end of input is the byte `0x04`, delivered in the
/// stream like any other: the bytes before it come out first, then a read returns 0 (end of input),
/// and what follows is kept for the read after that.
unsafe fn read_stream(fd: c_int, h: u32, buf: *mut u8, n: usize) -> isize {
    let tty = fd == 0 && TTY_IN.load(Ordering::Relaxed) != 0;
    let mut data: Vec<u8> = if tty {
        core::mem::take(&mut *PENDING.lock())
    } else {
        Vec::new()
    };
    if data.is_empty() {
        let mut tmp = alloc::vec![0u8; n.min(CHUNK)];
        match rt::io::read(h, &mut tmp) {
            Ok(0) | Err(_) => return 0,
            Ok(got) => {
                tmp.truncate(got);
                data = tmp;
            }
        }
    }
    let mut give = data.len().min(n);
    let mut eof = false;
    if tty {
        if let Some(i) = data.iter().position(|&b| b == 0x04) {
            if i == 0 {
                data.remove(0);
                give = 0;
                eof = true;
            } else {
                give = give.min(i);
            }
        }
    }
    core::ptr::copy_nonoverlapping(data.as_ptr(), buf, give);
    if tty && give < data.len() {
        *PENDING.lock() = data.split_off(give);
    }
    if eof {
        0
    } else {
        give as isize
    }
}
