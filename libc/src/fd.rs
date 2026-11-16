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

const MAX_FDS: usize = 256;
const CHUNK: usize = 16 * 1024;

pub struct OpenFile {
    fs: Fs,
    /// The absolute components of the path this was opened by: `openat` resolves relative to it, `..` included (a descriptor
    /// with no path cannot be the base of a path that climbs).
    path: Option<pathres::Comps>,
    off: u64,
    append: bool,
    readable: bool,
    writable: bool,
}

enum Entry {
    /// A standard stream: the handle of a socket.
    Stream(u32),
    File(OpenFile),
    /// A socket (S9 step 6): shared, so a blocking call does not hold the descriptor table.
    Socket(crate::socket::SockRef),
}

struct Table {
    fds: [Option<Entry>; MAX_FDS],
}

static FDS: Mutex<Table> = Mutex::new(Table {
    fds: [const { None }; MAX_FDS],
});

/// The process's namespace table (`pathres`): absolute prefixes to handles (the startup message owns them), and the cwd as a
/// path string plus its handle (USER-VIEW-SPEC section 5).
static TABLE: Mutex<pathres::Table> = Mutex::new(pathres::Table {
    mounts: Vec::new(),
    cwd: None,
    cwd_handle: None,
});
/// A cwd handle `chdir` opened (closed when the next `chdir` replaces it); 0 if the cwd is still the startup message's.
static OWN_CWD: AtomicU32 = AtomicU32::new(0);

/// Install the namespace table (start-up).
pub fn set_table(t: pathres::Table) {
    *TABLE.lock() = t;
}

/// `ELOOP`, as a status of this layer (too many symlinks).
const LOOP: i32 = -999;

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

/// Put a socket in the lowest free descriptor slot (3 and up).
pub fn install_socket(s: crate::socket::SockRef) -> Option<c_int> {
    let mut t = FDS.lock();
    let slot = (3..MAX_FDS).find(|&i| t.fds[i].is_none())?;
    t.fds[slot] = Some(Entry::Socket(s));
    Some(slot as c_int)
}

/// The socket behind `fd`: `Ok(None)` for a descriptor that is not a socket, `Err(())` for one that is not open.
pub fn socket_of(fd: c_int) -> Result<Option<crate::socket::SockRef>, ()> {
    let t = FDS.lock();
    match t.fds.get(fd as usize).and_then(Option::as_ref) {
        Some(Entry::Socket(s)) => Ok(Some(s.clone())),
        Some(_) => Ok(None),
        None => Err(()),
    }
}

fn errno_of(status: i32) -> c_int {
    match status {
        ps::NOT_FOUND => 2,       // ENOENT
        ps::EXISTS => 17,         // EEXIST
        ps::NOT_EMPTY => 39,      // ENOTEMPTY
        ps::IS_DIRECTORY => 21,   // EISDIR
        ps::NOT_DIRECTORY => 20,  // ENOTDIR
        ps::NAME_TOO_LONG => 36,  // ENAMETOOLONG
        ps::QUOTA => 28,          // ENOSPC
        ps::BAD_REQUEST => 22,    // EINVAL
        ps::SYMLINK | LOOP => 40, // ELOOP
        ps::BAD_TARGET => 22,     // EINVAL: a link target the server refuses
        -3 => 13,                 // EACCES
        -5 => 12,                 // ENOMEM
        _ => 5,                   // EIO
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

fn stop_of(s: rt::fs::Stopped) -> pathres::Stop {
    match s {
        rt::fs::Stopped::Link { consumed, target } => pathres::Stop::Symlink { consumed, target },
        rt::fs::Stopped::Status(e) => pathres::Stop::Status(e),
    }
}

fn status_of(f: pathres::Fail) -> i32 {
    match f {
        pathres::Fail::Status(s) => s,
        pathres::Fail::NoMount => ps::NOT_FOUND,
        pathres::Fail::Loop => LOOP,
    }
}

/// Open `path` with `flags` and `rights`: `..` is resolved lexically against the table and symlinks are followed here (the
/// server only reports them). Returns the connection and the absolute components of what was opened.
fn open_path(path: &[u8], flags: u16, rights: u32) -> Result<(Fs, pathres::Comps), i32> {
    let table = TABLE.lock().clone();
    pathres::open_following(&table, path, |h, rel| {
        dir_view(h).open_stop(rel, flags, rights).map_err(stop_of)
    })
    .map_err(status_of)
}

unsafe fn path_bytes<'a>(p: *const c_char) -> &'a [u8] {
    core::ffi::CStr::from_ptr(p).to_bytes()
}

pub const O_DIRECTORY: c_int = 0x10000;
pub const AT_FDCWD: c_int = -100;

#[no_mangle]
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, _mode: c_int) -> c_int {
    open_bytes(path_bytes(path), flags)
}

fn open_bytes(path: &[u8], flags: c_int) -> c_int {
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
    if flags & O_DIRECTORY != 0 {
        f |= pf::DIRECTORY;
        r |= rights::ENUMERATE;
    }
    let mut t = FDS.lock();
    let Some(slot) = (3..MAX_FDS).find(|&i| t.fds[i].is_none()) else {
        errno::set(24); // EMFILE
        return -1;
    };
    match open_path(path, f, r) {
        Ok((mut fs, abs)) => {
            // an append stream starts at the end (so `ftell` is right before the first write)
            let off = if flags & O_APPEND != 0 {
                fs.size().unwrap_or(0)
            } else {
                0
            };
            t.fds[slot] = Some(Entry::File(OpenFile {
                fs,
                path: Some(abs),
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

/// `openat`: `path` relative to the directory `dirfd` (or the cwd for `AT_FDCWD`). A relative path that climbs (`..`) works only
/// from a descriptor that remembers the path it was opened by; a descriptor without one refuses it.
#[no_mangle]
pub unsafe extern "C" fn openat(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    _mode: c_int,
) -> c_int {
    let p = path_bytes(path);
    if p.first() == Some(&b'/') || dirfd == AT_FDCWD {
        return open_bytes(p, flags);
    }
    let base = {
        let t = FDS.lock();
        match t.fds.get(dirfd as usize).and_then(Option::as_ref) {
            Some(Entry::File(f)) => f.path.clone(),
            _ => {
                errno::set(9); // EBADF
                return -1;
            }
        }
    };
    let Some(base) = base else {
        errno::set(13); // EACCES: no path to resolve from
        return -1;
    };
    let full = pathres::join_abs(&pathres::clamp(&base, p));
    open_bytes(&full, flags)
}

/// `chdir`: open the directory (following links), make it the cwd and remember its path.
#[no_mangle]
pub unsafe extern "C" fn chdir(path: *const c_char) -> c_int {
    match open_path(
        path_bytes(path),
        pf::DIRECTORY,
        rights::READ | rights::ENUMERATE,
    ) {
        Ok((fs, abs)) => {
            let old = OWN_CWD.swap(fs.ch, Ordering::Relaxed);
            let mut t = TABLE.lock();
            t.cwd = Some(abs);
            t.cwd_handle = Some(fs.ch);
            drop(t);
            if old != 0 {
                rt::sys::handle_close(old);
            }
            0
        }
        Err(e) => fail(e),
    }
}

/// `getcwd`: the cwd as an absolute path string; `ENOENT` if the program was not told its cwd.
#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut c_char, size: usize) -> *mut c_char {
    let cwd = TABLE.lock().cwd.clone();
    let Some(cwd) = cwd else {
        errno::set(2);
        return core::ptr::null_mut();
    };
    let p = pathres::join_abs(&cwd);
    if size < p.len() + 1 {
        errno::set(34); // ERANGE
        return core::ptr::null_mut();
    }
    core::ptr::copy_nonoverlapping(p.as_ptr(), buf as *mut u8, p.len());
    *buf.add(p.len()) = 0;
    buf
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
        Some(Entry::Socket(s)) => {
            drop(t);
            crate::socket::sock_close(s);
            0
        }
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
        Entry::Socket(s) => {
            let s = s.clone();
            drop(t); // a blocking read must not hold the table
            let out = core::slice::from_raw_parts_mut(buf as *mut u8, n);
            crate::socket::sock_recv(&s, out, false, None)
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
        Entry::Socket(s) => {
            let s = s.clone();
            drop(t);
            crate::socket::sock_send(&s, data, false, None)
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

/// `fcntl`: the descriptor flags a socket supports (`O_NONBLOCK`); other descriptors answer the harmless defaults.
#[no_mangle]
pub extern "C" fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int {
    match socket_of(fd) {
        Ok(Some(s)) => crate::socket::sock_fcntl(&s, cmd, arg),
        Ok(None) => match cmd {
            1..=4 => 0,
            _ => fail(-4),
        },
        Err(()) => {
            errno::set(9);
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
/// is opened (links followed, `..` lexical) with `parent_rights` only (reading and writing, not all four).
fn with_parent<R>(
    path: &[u8],
    parent_rights: u32,
    f: impl FnOnce(&mut Fs, &[u8]) -> Result<R, i32>,
) -> Result<R, i32> {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    let path = &path[..end];
    let (parent, name): (&[u8], &[u8]) = match path.iter().rposition(|&c| c == b'/') {
        None => (b".", path),
        Some(0) => (b"/", &path[1..]),
        Some(i) => (&path[..i], &path[i + 1..]),
    };
    if name.is_empty() || name == b"." || name == b".." {
        return Err(ps::BAD_REQUEST);
    }
    let (mut dir, _) = open_path(parent, pf::DIRECTORY, parent_rights)?;
    let r = f(&mut dir, name);
    dir.close();
    r
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
    let r = with_parent(path_bytes(path), rights::READ | rights::WRITE, |d, name| {
        d.mkdir(name)
    });
    match r {
        Ok(()) => {
            // the mode (less the mask) is an advisory attribute; the default needs no record
            let want = _mode as u32 & !UMASK.load(Ordering::Relaxed) & 0o7777;
            if want != 0o755 {
                let _ = set_posix(
                    path_bytes(path),
                    proto::file::ATTR_POSIX_MODE,
                    u64::from(want),
                );
            }
            0
        }
        Err(e) => fail(e),
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// `struct stat` (USER-VIEW-SPEC section 6): the mode, owner and group are the node's advisory `posix.*` attributes or defaults
/// (directories `0755`, files `0644`, links `0777`; the owner and group of the session); `st_nlink` is 1 (hard links are refused);
/// `st_dev` and `st_ino` are opaque tokens (the mount the node was found through, and a hash of its path), never ids a server
/// or a disk format uses.
#[repr(C)]
pub struct Stat {
    pub st_dev: u64,
    pub st_ino: u64,
    pub st_mode: u32,
    pub st_nlink: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub st_rdev: u64,
    pub st_size: i64,
    pub st_blksize: i64,
    pub st_blocks: i64,
    pub st_atim: Timespec,
    pub st_mtim: Timespec,
    pub st_ctim: Timespec,
}

pub const S_IFDIR: u32 = 0o040_000;
pub const S_IFREG: u32 = 0o100_000;
pub const S_IFLNK: u32 = 0o120_000;
pub const S_IFCHR: u32 = 0o020_000;
pub const S_IFIFO: u32 = 0o010_000;
pub const S_IFSOCK: u32 = 0o140_000;

/// The owner and group programs see when a node has no `posix.uid` or `posix.gid` (the session's: `UID` and `GID` in the
/// environment, else 1000).
pub static DEFAULT_UID: AtomicU32 = AtomicU32::new(1000);
pub static DEFAULT_GID: AtomicU32 = AtomicU32::new(1000);
/// The file-creation mask (`umask`).
static UMASK: AtomicU32 = AtomicU32::new(0o022);

fn fnv(parts: &[Vec<u8>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for p in parts {
        for &b in p.iter().chain(b"/") {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// Fill a `struct stat` from what `STAT` said, found through mount `dev` at absolute path `abs`.
unsafe fn fill_stat(out: *mut Stat, info: &proto::file::StatInfo, dev: u64, abs: &[Vec<u8>]) {
    use proto::file::ABSENT;
    let (kind_bits, default_perm) = match info.kind {
        2 => (S_IFDIR, 0o755),
        3 => (S_IFLNK, 0o777),
        _ => (S_IFREG, 0o644),
    };
    let perm = if info.mode == ABSENT {
        default_perm
    } else {
        info.mode & 0o7777
    };
    let secs = (info.mtime / 1_000_000_000) as i64;
    let nanos = (info.mtime % 1_000_000_000) as i64;
    let t = Timespec {
        tv_sec: secs,
        tv_nsec: nanos,
    };
    *out = Stat {
        st_dev: dev,
        st_ino: fnv(abs),
        st_mode: kind_bits | perm,
        st_nlink: 1,
        st_uid: if info.uid == ABSENT {
            DEFAULT_UID.load(Ordering::Relaxed)
        } else {
            info.uid
        },
        st_gid: if info.gid == ABSENT {
            DEFAULT_GID.load(Ordering::Relaxed)
        } else {
            info.gid
        },
        st_rdev: 0,
        st_size: info.size as i64,
        st_blksize: 4096,
        st_blocks: ((info.size + 511) / 512) as i64,
        st_atim: t,
        st_mtim: t,
        st_ctim: t,
    };
}

/// `stat` and `lstat`: one round trip to the server (`STAT` carries the attributes). `nofollow` describes a final symlink itself.
unsafe fn stat_path(path: *const c_char, out: *mut Stat, nofollow: bool) -> c_int {
    let _guard = FDS.lock();
    let table = TABLE.lock().clone();
    let fl = if nofollow { pf::NOFOLLOW } else { 0 };
    let r = pathres::open_following(&table, path_bytes(path), |h, rel| {
        dir_view(h)
            .stat_stop(rel, fl)
            .map(|s| (s, h))
            .map_err(stop_of)
    })
    .map_err(status_of);
    match r {
        Ok(((s, h), abs)) => {
            fill_stat(out, &s.info, u64::from(h), &abs);
            0
        }
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn stat(path: *const c_char, out: *mut Stat) -> c_int {
    stat_path(path, out, false)
}

#[no_mangle]
pub unsafe extern "C" fn lstat(path: *const c_char, out: *mut Stat) -> c_int {
    stat_path(path, out, true)
}

#[no_mangle]
pub unsafe extern "C" fn fstat(fd: c_int, out: *mut Stat) -> c_int {
    let mut t = FDS.lock();
    match t.fds.get_mut(fd as usize).and_then(Option::as_mut) {
        Some(Entry::File(f)) => match f.fs.fstat() {
            Ok(s) => {
                let abs = f.path.clone().unwrap_or_default();
                fill_stat(out, &s.info, 0, &abs);
                0
            }
            Err(e) => fail(e),
        },
        Some(Entry::Stream(_)) => {
            let info = proto::file::StatInfo {
                kind: 1,
                size: 0,
                mtime: 0,
                mode: 0o620,
                uid: proto::file::ABSENT,
                gid: proto::file::ABSENT,
            };
            fill_stat(out, &info, 0, &[]);
            let tty = TTY_OUT.load(Ordering::Relaxed) != 0;
            (*out).st_mode = if tty {
                S_IFCHR | 0o620
            } else {
                S_IFIFO | 0o600
            };
            0
        }
        Some(Entry::Socket(_)) => {
            let info = proto::file::StatInfo {
                kind: 1,
                size: 0,
                mtime: 0,
                mode: 0o777,
                uid: proto::file::ABSENT,
                gid: proto::file::ABSENT,
            };
            fill_stat(out, &info, 0, &[]);
            (*out).st_mode = S_IFSOCK | 0o777;
            0
        }
        None => {
            errno::set(9); // EBADF
            -1
        }
    }
}

/// Open a node (file, directory or link target) with `rights`, following links: a file first, then as a directory if that is
/// what it is (`chmod` and `chown` do not know which).
fn open_node(path: &[u8], rights_wanted: u32) -> Result<Fs, i32> {
    match open_path(path, 0, rights_wanted) {
        Err(ps::IS_DIRECTORY) => open_path(path, pf::DIRECTORY, rights_wanted).map(|(f, _)| f),
        Ok((f, _)) => Ok(f),
        Err(e) => Err(e),
    }
}

/// Set advisory attribute `name` of the node at `path` to `value` (a `U64`).
fn set_posix(path: &[u8], name: &[u8], value: u64) -> Result<(), i32> {
    let mut f = open_node(path, rights::READ | rights::WRITE)?;
    let r = f.attr_set(2, name, &value.to_le_bytes());
    f.close();
    r
}

#[no_mangle]
pub unsafe extern "C" fn chmod(path: *const c_char, mode: u32) -> c_int {
    let _guard = FDS.lock();
    match set_posix(
        path_bytes(path),
        proto::file::ATTR_POSIX_MODE,
        u64::from(mode & 0o7777),
    ) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

#[no_mangle]
pub unsafe extern "C" fn chown(path: *const c_char, uid: c_int, gid: c_int) -> c_int {
    let _guard = FDS.lock();
    let p = path_bytes(path);
    for (v, name) in [
        (uid, proto::file::ATTR_POSIX_UID),
        (gid, proto::file::ATTR_POSIX_GID),
    ] {
        if v != -1 {
            if let Err(e) = set_posix(p, name, v as u32 as u64) {
                return fail(e);
            }
        }
    }
    0
}

/// `fchmod`: the descriptor's own connection is used, so it needs write access (a read-only descriptor gets `EACCES`).
#[no_mangle]
pub extern "C" fn fchmod(fd: c_int, mode: u32) -> c_int {
    let mut t = FDS.lock();
    match t.fds.get_mut(fd as usize).and_then(Option::as_mut) {
        Some(Entry::File(f)) => match f.fs.attr_set(
            2,
            proto::file::ATTR_POSIX_MODE,
            &u64::from(mode & 0o7777).to_le_bytes(),
        ) {
            Ok(()) => 0,
            Err(e) => fail(e),
        },
        _ => {
            errno::set(9);
            -1
        }
    }
}

#[no_mangle]
pub extern "C" fn fchown(fd: c_int, uid: c_int, gid: c_int) -> c_int {
    let mut t = FDS.lock();
    match t.fds.get_mut(fd as usize).and_then(Option::as_mut) {
        Some(Entry::File(f)) => {
            for (v, name) in [
                (uid, proto::file::ATTR_POSIX_UID),
                (gid, proto::file::ATTR_POSIX_GID),
            ] {
                if v != -1 {
                    if let Err(e) = f.fs.attr_set(2, name, &(v as u32 as u64).to_le_bytes()) {
                        return fail(e);
                    }
                }
            }
            0
        }
        _ => {
            errno::set(9);
            -1
        }
    }
}

/// Set the file-creation mask; returns the old one.
#[no_mangle]
pub extern "C" fn umask(mask: u32) -> u32 {
    UMASK.swap(mask & 0o777, Ordering::Relaxed)
}

pub const X_OK: c_int = 1;

/// `access`: the node must exist; read and write are decided by the rights of the handles (so they are not refused here); the
/// execute bit is advisory data, read as such (`PATH` lookup, `test -x`).
#[no_mangle]
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    let mut st: Stat = core::mem::zeroed();
    if stat(path, &mut st) != 0 {
        return -1;
    }
    if mode & X_OK != 0 && st.st_mode & 0o111 == 0 {
        errno::set(13); // EACCES
        return -1;
    }
    0
}

/// `readlink`: the target text of a symlink, not NUL-terminated, truncated to `size`.
#[no_mangle]
pub unsafe extern "C" fn readlink(path: *const c_char, buf: *mut c_char, size: usize) -> isize {
    let _guard = FDS.lock();
    match with_parent(path_bytes(path), rights::READ, |d, name| d.readlink(name)) {
        Ok(t) => {
            let n = t.len().min(size);
            core::ptr::copy_nonoverlapping(t.as_ptr(), buf as *mut u8, n);
            n as isize
        }
        Err(e) => fail(e) as isize,
    }
}

/// `symlink`: the server refuses a target that is absolute or climbs above the connection root (`EINVAL`).
#[no_mangle]
pub unsafe extern "C" fn symlink(target: *const c_char, linkpath: *const c_char) -> c_int {
    let _guard = FDS.lock();
    let t = path_bytes(target);
    match with_parent(
        path_bytes(linkpath),
        rights::READ | rights::WRITE | rights::CREATE,
        |d, name| d.symlink(name, t),
    ) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
}

/// Hard links are refused (`EPERM`): one entry names one node, so `st_nlink` is always 1 and `tar` falls back to a copy.
#[no_mangle]
pub extern "C" fn link(_old: *const c_char, _new: *const c_char) -> c_int {
    errno::set(1); // EPERM
    -1
}

#[no_mangle]
pub unsafe extern "C" fn rename(from: *const c_char, to: *const c_char) -> c_int {
    let _guard = FDS.lock();
    let table = TABLE.lock().clone();
    let place = |p: &[u8]| {
        table
            .place(p)
            .map(|pl| (pl.handle, pathres::join_rel(&pl.rel)))
    };
    let (Some((d1, old)), Some((d2, new))) = (place(path_bytes(from)), place(path_bytes(to)))
    else {
        errno::set(2); // ENOENT
        return -1;
    };
    if d1 != d2 {
        errno::set(18); // EXDEV: the two paths are in different namespace entries
        return -1;
    }
    match dir_view(d1).rename(&old, &new) {
        Ok(()) => 0,
        Err(e) => fail(e),
    }
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
