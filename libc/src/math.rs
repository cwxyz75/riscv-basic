//! `<math.h>` over the `libm` crate (decision OD27).

macro_rules! unary {
    ($($c:ident => $r:path),* $(,)?) => {
        $(#[no_mangle] pub extern "C" fn $c(x: f64) -> f64 { $r(x) })*
    };
}

unary! {
    sin => libm::sin, cos => libm::cos, tan => libm::tan,
    asin => libm::asin, acos => libm::acos, atan => libm::atan,
    sinh => libm::sinh, cosh => libm::cosh, tanh => libm::tanh,
    exp => libm::exp, expm1 => libm::expm1, log => libm::log, log10 => libm::log10, log2 => libm::log2,
    log1p => libm::log1p, sqrt => libm::sqrt, cbrt => libm::cbrt,
    ceil => libm::ceil, floor => libm::floor, trunc => libm::trunc, round => libm::round,
    fabs => libm::fabs,
}

#[no_mangle]
pub extern "C" fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}
#[no_mangle]
pub extern "C" fn pow(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}
#[no_mangle]
pub extern "C" fn fmod(x: f64, y: f64) -> f64 {
    libm::fmod(x, y)
}
#[no_mangle]
pub extern "C" fn hypot(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}
#[no_mangle]
pub extern "C" fn copysign(x: f64, y: f64) -> f64 {
    libm::copysign(x, y)
}
#[no_mangle]
pub extern "C" fn fmin(x: f64, y: f64) -> f64 {
    libm::fmin(x, y)
}
#[no_mangle]
pub extern "C" fn fmax(x: f64, y: f64) -> f64 {
    libm::fmax(x, y)
}
#[no_mangle]
pub extern "C" fn ldexp(x: f64, e: core::ffi::c_int) -> f64 {
    libm::ldexp(x, e)
}
#[no_mangle]
pub unsafe extern "C" fn frexp(x: f64, e: *mut core::ffi::c_int) -> f64 {
    let (m, ex) = libm::frexp(x);
    *e = ex;
    m
}
#[no_mangle]
pub unsafe extern "C" fn modf(x: f64, ip: *mut f64) -> f64 {
    let (frac, int) = libm::modf(x);
    *ip = int;
    frac
}
