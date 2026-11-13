//! `FILE`: buffered streams over the fd layer (S7c-3). `stdin`, `stdout` and `stderr` are static;
//! `fopen` makes heap `FILE`s. Buffering is the C default: standard output is line buffered on a
//! terminal and fully buffered otherwise, standard error is unbuffered, files are fully buffered.
//! Every open stream is flushed at `exit`.

use crate::errno;
use crate::fd;
use core::ffi::{c_char, c_int, c_long, c_void};
use core::sync::atomic::Ordering;
use rt::sync::Mutex;

pub const BUFSIZ: usize = 4096;
const EOF: c_int = -1;

const F_EOF: u32 = 1;
const F_ERR: u32 = 2;
const F_READ: u32 = 4;
const F_WRITE: u32 = 8;
/// Buffering: 0 full, 1 line, 2 none (stored in bits 4-5).
const MODE_SHIFT: u32 = 4;
const MODE_MASK: u32 = 3 << MODE_SHIFT;
const F_HEAP: u32 = 64;

#[repr(C)]
pub struct File {
    fd: c_int,
    flags: u32,
    buf: *mut u8,
    cap: usize,
    /// Read side: bytes `rpos..rlen` of `buf` are unread. Write side: `buf[..wlen]` is unwritten.
    rpos: usize,
    rlen: usize,
    wlen: usize,
    ungot: c_int,
}

impl File {
    const fn std(fd: c_int, flags: u32) -> File {
        File {
            fd,
            flags,
            buf: core::ptr::null_mut(),
            cap: 0,
            rpos: 0,
            rlen: 0,
            wlen: 0,
            ungot: -1,
        }
    }

    fn mode(&self) -> u32 {
        (self.flags & MODE_MASK) >> MODE_SHIFT
    }

    fn set_mode(&mut self, m: u32) {
        self.flags = (self.flags & !MODE_MASK) | (m << MODE_SHIFT);
    }

    /// Make sure there is a buffer (allocated on first use).
    fn ensure_buf(&mut self) -> bool {
        if self.buf.is_null() {
            let p = crate::stdlib::malloc(BUFSIZ) as *mut u8;
            if p.is_null() {
                return false;
            }
            self.buf = p;
            self.cap = BUFSIZ;
        }
        true
    }

    /// Write out the pending output.
    fn flush(&mut self) -> bool {
        let mut done = 0;
        while done < self.wlen {
            // SAFETY: `buf[..wlen]` is initialised output.
            let n = unsafe {
                fd::write(
                    self.fd,
                    self.buf.add(done) as *const c_void,
                    self.wlen - done,
                )
            };
            if n <= 0 {
                self.flags |= F_ERR;
                self.wlen = 0;
                return false;
            }
            done += n as usize;
        }
        self.wlen = 0;
        true
    }

    fn put(&mut self, data: &[u8]) -> bool {
        if self.flags & F_READ != 0 && self.flags & F_WRITE == 0 {
            errno::set(9);
            self.flags |= F_ERR;
            return false;
        }
        // switching from reading to writing: drop the read-ahead, and move the fd offset back
        if self.rlen > self.rpos || self.ungot >= 0 {
            self.discard_read_ahead();
        }
        if self.mode() == 2 {
            // unbuffered
            let mut done = 0;
            while done < data.len() {
                // SAFETY: `data[done..]` is a valid slice.
                let n = unsafe {
                    fd::write(
                        self.fd,
                        data[done..].as_ptr() as *const c_void,
                        data.len() - done,
                    )
                };
                if n <= 0 {
                    self.flags |= F_ERR;
                    return false;
                }
                done += n as usize;
            }
            return true;
        }
        if !self.ensure_buf() {
            self.flags |= F_ERR;
            return false;
        }
        let mut rest = data;
        while !rest.is_empty() {
            let room = self.cap - self.wlen;
            let k = room.min(rest.len());
            // SAFETY: `buf` has `cap` bytes and `wlen + k <= cap`.
            unsafe { core::ptr::copy_nonoverlapping(rest.as_ptr(), self.buf.add(self.wlen), k) };
            self.wlen += k;
            rest = &rest[k..];
            if self.wlen == self.cap && !self.flush() {
                return false;
            }
        }
        if self.mode() == 1 && data.contains(&b'\n') && !self.flush() {
            return false;
        }
        true
    }

    fn discard_read_ahead(&mut self) {
        let unread = (self.rlen - self.rpos) as c_long + c_long::from(self.ungot >= 0);
        if unread > 0 {
            fd::lseek(self.fd, -unread, fd::SEEK_CUR);
        }
        self.rpos = 0;
        self.rlen = 0;
        self.ungot = -1;
    }

    /// The next byte, or EOF.
    fn getc(&mut self) -> c_int {
        if self.ungot >= 0 {
            let c = self.ungot;
            self.ungot = -1;
            return c;
        }
        if self.flags & F_WRITE != 0 && self.flags & F_READ == 0 {
            errno::set(9);
            self.flags |= F_ERR;
            return EOF;
        }
        if self.wlen > 0 && !self.flush() {
            return EOF;
        }
        if self.rpos == self.rlen {
            if !self.ensure_buf() {
                self.flags |= F_ERR;
                return EOF;
            }
            // a terminal line or a pipe returns what there is: do not wait for a full buffer
            // SAFETY: `buf` has `cap` bytes.
            let n = unsafe { fd::read(self.fd, self.buf as *mut c_void, self.cap) };
            if n < 0 {
                self.flags |= F_ERR;
                return EOF;
            }
            if n == 0 {
                self.flags |= F_EOF;
                return EOF;
            }
            self.rpos = 0;
            self.rlen = n as usize;
        }
        // SAFETY: `rpos < rlen <= cap`.
        let c = unsafe { *self.buf.add(self.rpos) };
        self.rpos += 1;
        c_int::from(c)
    }
}

static mut STD: [File; 3] = [
    File::std(0, F_READ),
    File::std(1, F_WRITE),
    File::std(2, F_WRITE),
];

#[repr(transparent)]
pub struct FilePtr(*mut File);
// SAFETY: the pointers name the static streams; every access follows C's rules for FILE (no
// concurrent use of one stream without the caller's own locking).
unsafe impl Sync for FilePtr {}

#[no_mangle]
pub static stdin: FilePtr = FilePtr(core::ptr::addr_of_mut!(STD) as *mut File);
#[no_mangle]
pub static stdout: FilePtr = FilePtr(unsafe { (core::ptr::addr_of_mut!(STD) as *mut File).add(1) });
#[no_mangle]
pub static stderr: FilePtr = FilePtr(unsafe { (core::ptr::addr_of_mut!(STD) as *mut File).add(2) });

fn std_stream(i: usize) -> *mut File {
    // SAFETY: `STD` has three elements.
    unsafe { (core::ptr::addr_of_mut!(STD) as *mut File).add(i) }
}

/// Set the default buffering of the standard streams (once the environment is known).
pub fn init_std_buffering() {
    // SAFETY: start-up, single thread.
    unsafe {
        let out = &mut *std_stream(1);
        out.set_mode(if fd::TTY_OUT.load(Ordering::Relaxed) != 0 {
            1
        } else {
            0
        });
        (*std_stream(2)).set_mode(2);
    }
}

/// Every stream `fopen` made (the standard ones are static).
struct OpenList([*mut File; 64]);
// SAFETY: guarded by the mutex; the pointers are FILEs this module allocated.
unsafe impl Send for OpenList {}
static OPEN: Mutex<OpenList> = Mutex::new(OpenList([core::ptr::null_mut(); 64]));

/// Flush every output stream (`fflush(NULL)`, and at exit).
pub fn flush_all() {
    for i in 0..3 {
        // SAFETY: the static streams.
        unsafe { (*std_stream(i)).flush() };
    }
    let list = OPEN.lock();
    for &f in list.0.iter().filter(|f| !f.is_null()) {
        // SAFETY: a registered live FILE.
        unsafe { (*f).flush() };
    }
}

pub fn write_stderr(bytes: &[u8]) {
    // SAFETY: the static stderr stream.
    unsafe { (*std_stream(2)).put(bytes) };
}

/// `fopen`'s mode string as open(2) flags and `FILE` flags.
unsafe fn parse_mode(mode: *const c_char) -> Option<(c_int, u32)> {
    let m = core::ffi::CStr::from_ptr(mode).to_bytes();
    let plus = m.contains(&b'+');
    Some(match m.first() {
        Some(b'r') => (
            if plus { fd::O_RDWR } else { fd::O_RDONLY },
            F_READ | if plus { F_WRITE } else { 0 },
        ),
        Some(b'w') => (
            (if plus { fd::O_RDWR } else { fd::O_WRONLY }) | fd::O_CREAT | fd::O_TRUNC,
            F_WRITE | if plus { F_READ } else { 0 },
        ),
        Some(b'a') => (
            (if plus { fd::O_RDWR } else { fd::O_WRONLY }) | fd::O_CREAT | fd::O_APPEND,
            F_WRITE | if plus { F_READ } else { 0 },
        ),
        _ => return None,
    })
}

#[no_mangle]
pub unsafe extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut File {
    let Some((flags, fl)) = parse_mode(mode) else {
        errno::set(22);
        return core::ptr::null_mut();
    };
    let n = fd::open(path, flags, 0);
    if n < 0 {
        return core::ptr::null_mut();
    }
    adopt(n, fl)
}

/// Wrap an open fd in a new heap `FILE` (closing the fd if that fails).
fn adopt(n: c_int, fl: u32) -> *mut File {
    let p = crate::stdlib::malloc(core::mem::size_of::<File>()) as *mut File;
    if p.is_null() {
        fd::close(n);
        return p;
    }
    // SAFETY: `p` is fresh memory of the right size and alignment.
    unsafe {
        p.write(File {
            fd: n,
            flags: fl | F_HEAP,
            buf: core::ptr::null_mut(),
            cap: 0,
            rpos: 0,
            rlen: 0,
            wlen: 0,
            ungot: -1,
        });
    }
    let mut list = OPEN.lock();
    match list.0.iter_mut().find(|s| s.is_null()) {
        Some(slot) => *slot = p,
        None => {
            // more streams than the table holds: refuse (EMFILE), as the fd table would
            drop(list);
            fd::close(n);
            // SAFETY: `p` was allocated by `malloc` just above.
            unsafe { crate::stdlib::free(p as *mut c_void) };
            errno::set(24);
            return core::ptr::null_mut();
        }
    }
    p
}

/// Reuse `f` for another file (the stream object stays: `freopen(p, m, stdout)` redirects standard
/// output, and a heap stream's pointer stays valid).
#[no_mangle]
pub unsafe extern "C" fn freopen(
    path: *const c_char,
    mode: *const c_char,
    f: *mut File,
) -> *mut File {
    if path.is_null() {
        errno::set(22); // changing the mode of an open stream is not supported
        return core::ptr::null_mut();
    }
    let Some((flags, fl)) = parse_mode(mode) else {
        errno::set(22);
        return core::ptr::null_mut();
    };
    (*f).flush();
    let n = fd::open(path, flags, 0);
    if n < 0 {
        fclose(f);
        return core::ptr::null_mut();
    }
    // `fd::open` filled the slot `n`; put the stream on it and release the old descriptor
    fd::close((*f).fd);
    (*f).fd = n;
    (*f).flags = ((*f).flags & (F_HEAP | MODE_MASK)) | fl;
    (*f).rpos = 0;
    (*f).rlen = 0;
    (*f).wlen = 0;
    (*f).ungot = -1;
    f
}

#[no_mangle]
pub unsafe extern "C" fn fclose(f: *mut File) -> c_int {
    let ok = (*f).flush();
    fd::close((*f).fd);
    if (*f).flags & F_HEAP != 0 {
        {
            let mut list = OPEN.lock();
            if let Some(slot) = list.0.iter_mut().find(|s| **s == f) {
                *slot = core::ptr::null_mut();
            }
        }
        crate::stdlib::free((*f).buf as *mut c_void);
        crate::stdlib::free(f as *mut c_void);
    }
    if ok {
        0
    } else {
        EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn fflush(f: *mut File) -> c_int {
    if f.is_null() {
        flush_all();
        return 0;
    }
    if (*f).flush() {
        0
    } else {
        EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn fwrite(p: *const c_void, size: usize, n: usize, f: *mut File) -> usize {
    let total = size.saturating_mul(n);
    if total == 0 {
        return 0;
    }
    if (*f).put(core::slice::from_raw_parts(p as *const u8, total)) {
        n
    } else {
        0
    }
}

#[no_mangle]
pub unsafe extern "C" fn fread(p: *mut c_void, size: usize, n: usize, f: *mut File) -> usize {
    let total = size.saturating_mul(n);
    if total == 0 {
        return 0;
    }
    let out = p as *mut u8;
    for i in 0..total {
        let c = (*f).getc();
        if c == EOF {
            return i / size;
        }
        *out.add(i) = c as u8;
    }
    n
}

#[no_mangle]
pub unsafe extern "C" fn fgetc(f: *mut File) -> c_int {
    (*f).getc()
}

#[no_mangle]
pub unsafe extern "C" fn getc(f: *mut File) -> c_int {
    (*f).getc()
}

#[no_mangle]
pub unsafe extern "C" fn getchar() -> c_int {
    (*std_stream(0)).getc()
}

#[no_mangle]
pub unsafe extern "C" fn ungetc(c: c_int, f: *mut File) -> c_int {
    if c == EOF || (*f).ungot >= 0 {
        return EOF;
    }
    (*f).ungot = c & 0xff;
    (*f).flags &= !F_EOF;
    c & 0xff
}

#[no_mangle]
pub unsafe extern "C" fn fgets(s: *mut c_char, n: c_int, f: *mut File) -> *mut c_char {
    if n <= 0 {
        return core::ptr::null_mut();
    }
    let mut i = 0usize;
    while i + 1 < n as usize {
        let c = (*f).getc();
        if c == EOF {
            break;
        }
        *s.add(i) = c as c_char;
        i += 1;
        if c == c_int::from(b'\n') {
            break;
        }
    }
    if i == 0 {
        return core::ptr::null_mut();
    }
    *s.add(i) = 0;
    s
}

#[no_mangle]
pub unsafe extern "C" fn fputs(s: *const c_char, f: *mut File) -> c_int {
    if (*f).put(core::ffi::CStr::from_ptr(s).to_bytes()) {
        0
    } else {
        EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn puts(s: *const c_char) -> c_int {
    let out = &mut *std_stream(1);
    if out.put(core::ffi::CStr::from_ptr(s).to_bytes()) && out.put(b"\n") {
        1
    } else {
        EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn fputc(c: c_int, f: *mut File) -> c_int {
    if (*f).put(&[c as u8]) {
        c & 0xff
    } else {
        EOF
    }
}

#[no_mangle]
pub unsafe extern "C" fn putc(c: c_int, f: *mut File) -> c_int {
    fputc(c, f)
}

#[no_mangle]
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    fputc(c, std_stream(1))
}

#[no_mangle]
pub unsafe extern "C" fn feof(f: *mut File) -> c_int {
    c_int::from((*f).flags & F_EOF != 0)
}

#[no_mangle]
pub unsafe extern "C" fn ferror(f: *mut File) -> c_int {
    c_int::from((*f).flags & F_ERR != 0)
}

#[no_mangle]
pub unsafe extern "C" fn clearerr(f: *mut File) {
    (*f).flags &= !(F_EOF | F_ERR);
}

#[no_mangle]
pub unsafe extern "C" fn fileno(f: *mut File) -> c_int {
    (*f).fd
}

#[no_mangle]
pub unsafe extern "C" fn fseek(f: *mut File, off: c_long, whence: c_int) -> c_int {
    let f = &mut *f;
    if !f.flush() {
        return -1;
    }
    // SEEK_CUR is relative to the *logical* position: the read-ahead has not been consumed. Seek
    // first and drop the buffered input only if that worked (a failed seek changes nothing).
    let adjust = if whence == fd::SEEK_CUR {
        -((f.rlen - f.rpos) as c_long) - c_long::from(f.ungot >= 0)
    } else {
        0
    };
    if fd::lseek(f.fd, off + adjust, whence) < 0 {
        return -1;
    }
    f.rpos = 0;
    f.rlen = 0;
    f.ungot = -1;
    f.flags &= !F_EOF;
    0
}

#[no_mangle]
pub unsafe extern "C" fn ftell(f: *mut File) -> c_long {
    let f = &mut *f;
    let pos = fd::lseek(f.fd, 0, fd::SEEK_CUR);
    if pos < 0 {
        return -1;
    }
    pos - (f.rlen - f.rpos) as c_long - c_long::from(f.ungot >= 0) + f.wlen as c_long
}

#[no_mangle]
pub unsafe extern "C" fn rewind(f: *mut File) {
    fseek(f, 0, fd::SEEK_SET);
    clearerr(f);
}

#[no_mangle]
pub unsafe extern "C" fn setvbuf(
    f: *mut File,
    buf: *mut c_char,
    mode: c_int,
    size: usize,
) -> c_int {
    let f = &mut *f;
    if !f.flush() {
        return -1;
    }
    let _ = (buf, size); // the library keeps its own buffer
    match mode {
        0..=2 => {
            f.set_mode(mode as u32);
            0
        }
        _ => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn setbuf(f: *mut File, buf: *mut c_char) {
    setvbuf(f, buf, if buf.is_null() { 2 } else { 0 }, BUFSIZ);
}

#[no_mangle]
pub unsafe extern "C" fn tmpfile() -> *mut File {
    let mut name = fd::scratch_name();
    name.push(0);
    let n = fd::open(
        name.as_ptr() as *const c_char,
        fd::O_RDWR | fd::O_CREAT | fd::O_EXCL,
        0,
    );
    if n < 0 {
        return core::ptr::null_mut();
    }
    // the file lives until it is closed: unlink the name at once (the server keeps open files)
    fd::unlink(name.as_ptr() as *const c_char);
    adopt(n, F_READ | F_WRITE)
}

#[no_mangle]
pub unsafe extern "C" fn tmpnam(buf: *mut c_char) -> *mut c_char {
    static mut OWN: [u8; 64] = [0; 64];
    let mut name = fd::scratch_name();
    name.push(0);
    let dest = if buf.is_null() {
        core::ptr::addr_of_mut!(OWN) as *mut u8
    } else {
        buf as *mut u8
    };
    core::ptr::copy_nonoverlapping(name.as_ptr(), dest, name.len().min(63));
    *dest.add(name.len().min(63)) = 0;
    dest as *mut c_char
}

#[no_mangle]
pub unsafe extern "C" fn remove(path: *const c_char) -> c_int {
    fd::unlink(path)
}

#[no_mangle]
pub unsafe extern "C" fn perror(msg: *const c_char) {
    let e = crate::errno::message(*crate::errno::__errno_location());
    if !msg.is_null() && *msg != 0 {
        write_stderr(core::ffi::CStr::from_ptr(msg).to_bytes());
        write_stderr(b": ");
    }
    write_stderr(&e[..e.len() - 1]);
    write_stderr(b"\n");
}

#[no_mangle]
pub unsafe extern "C" fn vfprintf(f: *mut File, fmt: *const c_char, ap: *const u64) -> c_int {
    let Some(out) = crate::stdio::render(fmt, ap) else {
        return -1;
    };
    if (*f).put(&out) {
        out.len() as c_int
    } else {
        -1
    }
}

#[no_mangle]
pub unsafe extern "C" fn vprintf(fmt: *const c_char, ap: *const u64) -> c_int {
    vfprintf(std_stream(1), fmt, ap)
}
