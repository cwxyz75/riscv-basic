//! The terminal for programs (T124): `tcgetattr`, `tcsetattr`, `ioctl(TIOCGWINSZ)`, `tcflush`. A program does not hold the console
//! server's control channel: it asks the shell that started it (`proto::spawn::OP_TERM`), which answers a program of the foreground
//! job and puts the mode back when the job ends. The three flags the console server has (`ICANON`, `ECHO`, `ISIG`) are the whole
//! of what `c_lflag` can say; the rest of `struct termios` is the usual raw-terminal answer. `VMIN` and `VTIME` are kept here and
//! used by `read` on the terminal when `ICANON` is off.

use crate::errno;
use core::ffi::{c_int, c_ulong};
use core::sync::atomic::{AtomicU8, Ordering};
use proto::console::mode;
use proto::spawn::{TERM_GET_MODE, TERM_GET_SIZE, TERM_SET_MODE};

const ISIG: u32 = 0o1;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const VTIME: usize = 5;
const VMIN: usize = 6;

/// `VMIN` and `VTIME` as the program last set them (the defaults of a fresh terminal: 1 and 0).
pub static TERM_VMIN: AtomicU8 = AtomicU8::new(1);
pub static TERM_VTIME: AtomicU8 = AtomicU8::new(0);
/// Whether the program has turned `ICANON` off.
pub static TERM_RAWISH: AtomicU8 = AtomicU8::new(0);

#[repr(C)]
pub struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 32],
    c_ispeed: u32,
    c_ospeed: u32,
}

#[repr(C)]
pub struct Winsize {
    ws_row: u16,
    ws_col: u16,
    ws_xpixel: u16,
    ws_ypixel: u16,
}

fn need_tty(fd: c_int) -> bool {
    if crate::fd::is_tty(fd) {
        return true;
    }
    errno::set(25); // ENOTTY
    false
}

#[no_mangle]
pub unsafe extern "C" fn tcgetattr(fd: c_int, t: *mut Termios) -> c_int {
    if !need_tty(fd) {
        return -1;
    }
    let m = match crate::spawn::term_via_shell(TERM_GET_MODE, 0) {
        Ok(m) => m as u8,
        Err(e) => {
            errno::set(e);
            return -1;
        }
    };
    let mut cc = [0u8; 32];
    cc[0] = 3; // VINTR ^C
    cc[1] = 0x1c; // VQUIT
    cc[2] = 0x7f; // VERASE
    cc[3] = 0x15; // VKILL
    cc[4] = 4; // VEOF ^D
    cc[VTIME] = TERM_VTIME.load(Ordering::Relaxed);
    cc[VMIN] = TERM_VMIN.load(Ordering::Relaxed);
    *t = Termios {
        c_iflag: 0o400 | 0o2000, // ICRNL | IXON
        c_oflag: 0o1 | 0o4,      // OPOST | ONLCR
        c_cflag: 0o60 | 0o200,   // CS8 | CREAD
        c_lflag: (if m & mode::ISIG != 0 { ISIG } else { 0 })
            | (if m & mode::ICANON != 0 { ICANON } else { 0 })
            | (if m & mode::ECHO != 0 { ECHO } else { 0 }),
        c_line: 0,
        c_cc: cc,
        c_ispeed: 0o15,
        c_ospeed: 0o15,
    };
    0
}

#[no_mangle]
pub unsafe extern "C" fn tcsetattr(fd: c_int, _actions: c_int, t: *const Termios) -> c_int {
    if !need_tty(fd) {
        return -1;
    }
    let l = (*t).c_lflag;
    let m = (if l & ISIG != 0 { mode::ISIG } else { 0 })
        | (if l & ICANON != 0 { mode::ICANON } else { 0 })
        | (if l & ECHO != 0 { mode::ECHO } else { 0 });
    if let Err(e) = crate::spawn::term_via_shell(TERM_SET_MODE, u32::from(m)) {
        errno::set(e);
        return -1;
    }
    TERM_VMIN.store((*t).c_cc[VMIN], Ordering::Relaxed);
    TERM_VTIME.store((*t).c_cc[VTIME], Ordering::Relaxed);
    TERM_RAWISH.store(u8::from(l & ICANON == 0), Ordering::Relaxed);
    0
}

#[no_mangle]
pub extern "C" fn tcflush(fd: c_int, _queue: c_int) -> c_int {
    if !need_tty(fd) {
        return -1;
    }
    0
}

/// `ioctl`: the terminal requests; any other is `ENOTTY`. The third argument is a pointer for every request handled here.
#[no_mangle]
pub unsafe extern "C" fn ioctl(fd: c_int, request: c_ulong, arg: usize) -> c_int {
    match request {
        0x5401 => tcgetattr(fd, arg as *mut Termios),
        0x5402..=0x5404 => tcsetattr(fd, 0, arg as *const Termios),
        0x5413 => {
            if !need_tty(fd) {
                return -1;
            }
            match crate::spawn::term_via_shell(TERM_GET_SIZE, 0) {
                Ok(v) => {
                    *(arg as *mut Winsize) = Winsize {
                        ws_row: (v >> 16) as u16,
                        ws_col: v as u16,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    0
                }
                Err(e) => {
                    errno::set(e);
                    -1
                }
            }
        }
        _ => {
            errno::set(25);
            -1
        }
    }
}
