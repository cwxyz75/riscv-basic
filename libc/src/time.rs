//! `<time.h>` over `cfmt::time` (UTC only: there is no time zone database and no RTC yet, so
//! wall-clock time is the time since boot).

use core::ffi::{c_char, c_int, c_long};

pub type TimeT = i64;

#[repr(C)]
pub struct Tm {
    sec: c_int,
    min: c_int,
    hour: c_int,
    mday: c_int,
    mon: c_int,
    year: c_int,
    wday: c_int,
    yday: c_int,
    isdst: c_int,
    gmtoff: c_long,
    zone: *const c_char,
}

static ZONE: &[u8] = b"UTC\0";

fn to_c(t: &cfmt::time::Tm) -> Tm {
    Tm {
        sec: t.sec,
        min: t.min,
        hour: t.hour,
        mday: t.mday,
        mon: t.mon,
        year: t.year,
        wday: t.wday,
        yday: t.yday,
        isdst: 0,
        gmtoff: 0,
        zone: ZONE.as_ptr() as *const c_char,
    }
}

fn from_c(t: &Tm) -> cfmt::time::Tm {
    cfmt::time::Tm {
        sec: t.sec,
        min: t.min,
        hour: t.hour,
        mday: t.mday,
        mon: t.mon,
        year: t.year,
        wday: t.wday,
        yday: t.yday,
    }
}

#[no_mangle]
pub extern "C" fn time(out: *mut TimeT) -> TimeT {
    let t = (rt::sys::clock_get() / 1000) as TimeT; // seconds since boot
    if !out.is_null() {
        // SAFETY: the caller's time_t.
        unsafe { *out = t };
    }
    t
}

#[no_mangle]
pub extern "C" fn clock() -> c_long {
    (rt::sys::clock_get() * 1000) as c_long // CLOCKS_PER_SEC is 1,000,000
}

#[no_mangle]
pub extern "C" fn difftime(a: TimeT, b: TimeT) -> f64 {
    (a - b) as f64
}

#[no_mangle]
pub unsafe extern "C" fn gmtime_r(t: *const TimeT, out: *mut Tm) -> *mut Tm {
    match cfmt::time::gmtime(*t) {
        Some(tm) => {
            *out = to_c(&tm);
            out
        }
        None => {
            crate::errno::set(75); // EOVERFLOW
            core::ptr::null_mut()
        }
    }
}

static mut STATIC_TM: Tm = Tm {
    sec: 0,
    min: 0,
    hour: 0,
    mday: 0,
    mon: 0,
    year: 0,
    wday: 0,
    yday: 0,
    isdst: 0,
    gmtoff: 0,
    zone: core::ptr::null(),
};

#[no_mangle]
pub unsafe extern "C" fn gmtime(t: *const TimeT) -> *mut Tm {
    gmtime_r(t, core::ptr::addr_of_mut!(STATIC_TM))
}

#[no_mangle]
pub unsafe extern "C" fn localtime_r(t: *const TimeT, out: *mut Tm) -> *mut Tm {
    gmtime_r(t, out)
}

#[no_mangle]
pub unsafe extern "C" fn localtime(t: *const TimeT) -> *mut Tm {
    gmtime(t)
}

/// Normalises the fields in place, like `mktime`.
#[no_mangle]
pub unsafe extern "C" fn mktime(tm: *mut Tm) -> TimeT {
    match cfmt::time::timegm(&from_c(&*tm)).and_then(|t| Some((t, cfmt::time::gmtime(t)?))) {
        Some((t, norm)) => {
            *tm = to_c(&norm);
            t
        }
        None => -1,
    }
}

#[no_mangle]
pub unsafe extern "C" fn timegm(tm: *mut Tm) -> TimeT {
    mktime(tm)
}

#[no_mangle]
pub unsafe extern "C" fn strftime(
    buf: *mut c_char,
    max: usize,
    fmt: *const c_char,
    tm: *const Tm,
) -> usize {
    let mut out = alloc::vec::Vec::new();
    let f = core::ffi::CStr::from_ptr(fmt).to_bytes();
    if cfmt::time::strftime(f, &from_c(&*tm), &mut out).is_err() || out.len() + 1 > max {
        return 0;
    }
    core::ptr::copy_nonoverlapping(out.as_ptr(), buf as *mut u8, out.len());
    *buf.add(out.len()) = 0;
    out.len()
}
