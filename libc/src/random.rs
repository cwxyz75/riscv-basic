//! `getrandom` over the system random service (`/svc/random`, S9 step 5): bytes from a generator seeded by the entropy device.
//! A program whose `/svc` view lacks the name gets `EACCES`; a machine whose service is not seeded gets `EAGAIN`.
use crate::errno;
use core::ffi::c_void;
use rt::random::{Random, RandomError};
use rt::sync::Mutex;

static RAND: Mutex<Option<Random>> = Mutex::new(None);

#[no_mangle]
pub unsafe extern "C" fn getrandom(buf: *mut c_void, len: usize, _flags: u32) -> isize {
    if len == 0 {
        return 0;
    }
    if buf.is_null() {
        errno::set(14); // EFAULT
        return -1;
    }
    let n = len.min(256); // like Linux: a short count for a large request
    let mut g = RAND.lock();
    if g.is_none() {
        let h = crate::socket::SVC.load(core::sync::atomic::Ordering::Relaxed);
        if h == 0 {
            errno::set(13);
            return -1;
        }
        let mut svc = rt::fs::Fs::new_at(h, 1 << 24);
        match Random::connect(&mut svc, rt::sys::clock_get() + 2000) {
            Ok(r) => *g = Some(r),
            Err(_) => {
                errno::set(13); // EACCES: no such name in this program's view
                return -1;
            }
        }
    }
    let out = core::slice::from_raw_parts_mut(buf as *mut u8, n);
    match g.as_mut().unwrap().fill(out, rt::sys::clock_get() + 5000) {
        Ok(()) => n as isize,
        Err(RandomError::NotSeeded) => {
            errno::set(11); // EAGAIN: not seeded yet
            -1
        }
        Err(RandomError::Closed) => {
            *g = None;
            errno::set(5);
            -1
        }
        Err(_) => {
            errno::set(5);
            -1
        }
    }
}
