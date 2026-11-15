//! `getaddrinfo` and friends (S9 step 7, S9-SPEC 11.2): IPv4 only, numeric services only, answers from the stub resolver of
//! `rt::resolve` over `/svc/net`. `RESOLVER=ADDR:PORT` in the environment replaces the configured DNS server (the test hook).
use crate::errno;
use alloc::boxed::Box;
use core::ffi::{c_char, c_int, c_void, CStr};

pub const EAI_BADFLAGS: c_int = -1;
pub const EAI_NONAME: c_int = -2;
pub const EAI_AGAIN: c_int = -3;
pub const EAI_FAIL: c_int = -4;
pub const EAI_FAMILY: c_int = -6;
pub const EAI_SOCKTYPE: c_int = -7;
pub const EAI_SERVICE: c_int = -8;
pub const EAI_MEMORY: c_int = -10;
pub const EAI_SYSTEM: c_int = -11;

const AI_PASSIVE: c_int = 1;
const AI_NUMERICHOST: c_int = 4;

#[repr(C)]
pub struct AddrInfo {
    ai_flags: c_int,
    ai_family: c_int,
    ai_socktype: c_int,
    ai_protocol: c_int,
    ai_addrlen: u32,
    ai_addr: *mut c_void,
    ai_canonname: *mut c_char,
    ai_next: *mut AddrInfo,
}

fn parse_server(s: &[u8]) -> Option<([u8; 4], u16)> {
    let (a, p) = s
        .iter()
        .position(|&c| c == b':')
        .map(|i| (&s[..i], &s[i + 1..]))?;
    let addr = sockshim::inet_pton4(a)?;
    let port = core::str::from_utf8(p).ok()?.parse().ok()?;
    Some((addr, port))
}

#[no_mangle]
pub unsafe extern "C" fn getaddrinfo(
    node: *const c_char,
    service: *const c_char,
    hints: *const AddrInfo,
    res: *mut *mut AddrInfo,
) -> c_int {
    if res.is_null() {
        return EAI_SYSTEM;
    }
    let (flags, family, socktype) = if hints.is_null() {
        (0, 0, 0)
    } else {
        ((*hints).ai_flags, (*hints).ai_family, (*hints).ai_socktype)
    };
    if family != 0 && family != 2 {
        return EAI_FAMILY;
    }
    if !(0..=2).contains(&socktype) {
        return EAI_SOCKTYPE;
    }
    let port: u16 = if service.is_null() {
        0
    } else {
        let s = CStr::from_ptr(service).to_bytes();
        match core::str::from_utf8(s)
            .ok()
            .and_then(|t| t.parse::<u16>().ok())
        {
            Some(p) => p,
            None => return EAI_SERVICE, // numeric services only
        }
    };
    let addr: [u8; 4] = if node.is_null() {
        if flags & AI_PASSIVE != 0 {
            [0; 4]
        } else {
            [127, 0, 0, 1]
        }
    } else {
        let name = CStr::from_ptr(node).to_bytes();
        if let Some(a) = sockshim::inet_pton4(name) {
            a
        } else if flags & AI_NUMERICHOST != 0 {
            return EAI_NONAME;
        } else {
            let server = {
                let v = crate::stdlib::getenv(c"RESOLVER".as_ptr());
                if v.is_null() {
                    None
                } else {
                    parse_server(CStr::from_ptr(v).to_bytes())
                }
            };
            let deadline = rt::sys::clock_get() + 12_000;
            match crate::socket::with_net_pub(|n| {
                Ok(rt::resolve::resolve(n, name, server, deadline))
            }) {
                Ok(Ok(a)) => a,
                Ok(Err(rt::resolve::ResolveError::Timeout)) => return EAI_AGAIN,
                Ok(Err(
                    rt::resolve::ResolveError::NoSuchName
                    | rt::resolve::ResolveError::NoData
                    | rt::resolve::ResolveError::BadName,
                )) => return EAI_NONAME,
                Ok(Err(_)) => return EAI_FAIL,
                Err(e) => {
                    errno::set(e);
                    return EAI_SYSTEM;
                }
            }
        }
    };
    // one entry per socket type asked for (both, if none was)
    let types: &[c_int] = match socktype {
        1 => &[1],
        2 => &[2],
        _ => &[1, 2],
    };
    let mut head: *mut AddrInfo = core::ptr::null_mut();
    for &t in types.iter().rev() {
        let sa = Box::new(sockshim::format_sockaddr_in(addr, port));
        let ai = Box::new(AddrInfo {
            ai_flags: 0,
            ai_family: 2,
            ai_socktype: t,
            ai_protocol: if t == 1 { 6 } else { 17 },
            ai_addrlen: sockshim::SOCKADDR_IN_LEN as u32,
            ai_addr: Box::into_raw(sa) as *mut c_void,
            ai_canonname: core::ptr::null_mut(),
            ai_next: head,
        });
        head = Box::into_raw(ai);
    }
    *res = head;
    0
}

#[no_mangle]
pub unsafe extern "C" fn freeaddrinfo(mut ai: *mut AddrInfo) {
    while !ai.is_null() {
        let b = Box::from_raw(ai);
        if !b.ai_addr.is_null() {
            drop(Box::from_raw(
                b.ai_addr as *mut [u8; sockshim::SOCKADDR_IN_LEN],
            ));
        }
        ai = b.ai_next;
    }
}

#[no_mangle]
pub extern "C" fn gai_strerror(code: c_int) -> *const c_char {
    (match code {
        0 => &b"Success\0"[..],
        EAI_BADFLAGS => b"Bad value for ai_flags\0",
        EAI_NONAME => b"Name or service not known\0",
        EAI_AGAIN => b"Temporary failure in name resolution\0",
        EAI_FAIL => b"Non-recoverable failure in name resolution\0",
        EAI_FAMILY => b"ai_family not supported\0",
        EAI_SOCKTYPE => b"ai_socktype not supported\0",
        EAI_SERVICE => b"Servname not supported for ai_socktype\0",
        EAI_MEMORY => b"Memory allocation failure\0",
        _ => b"System error\0",
    })
    .as_ptr() as *const c_char
}
