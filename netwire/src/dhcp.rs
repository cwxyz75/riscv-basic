//! DHCP (RFC 2131): the BOOTP header and the options the client uses. Every option length is checked against the
//! bytes present; a truncated option ends the option list; options beyond the buffer are never read.
use crate::{arr, be16, be32, out, Ip, Mac, WireError};

/// The fixed BOOTP header (236 bytes) plus the magic cookie.
pub const FIXED: usize = 240;
pub const MAGIC: [u8; 4] = [99, 130, 83, 99];
pub const OP_REQUEST: u8 = 1;
pub const OP_REPLY: u8 = 2;
pub const PORT_SERVER: u16 = 67;
pub const PORT_CLIENT: u16 = 68;
/// The BOOTP broadcast flag.
pub const FLAG_BROADCAST: u16 = 0x8000;

pub mod msg {
    pub const DISCOVER: u8 = 1;
    pub const OFFER: u8 = 2;
    pub const REQUEST: u8 = 3;
    pub const DECLINE: u8 = 4;
    pub const ACK: u8 = 5;
    pub const NAK: u8 = 6;
    pub const RELEASE: u8 = 7;
    pub const INFORM: u8 = 8;
}

pub mod opt {
    pub const PAD: u8 = 0;
    pub const MASK: u8 = 1;
    pub const ROUTER: u8 = 3;
    pub const DNS: u8 = 6;
    pub const REQUESTED_IP: u8 = 50;
    pub const LEASE: u8 = 51;
    pub const MSG_TYPE: u8 = 53;
    pub const SERVER_ID: u8 = 54;
    pub const PARAM_LIST: u8 = 55;
    pub const T1: u8 = 58;
    pub const T2: u8 = 59;
    pub const CLIENT_ID: u8 = 61;
    pub const END: u8 = 255;
}

#[derive(Clone, Copy, Debug)]
pub struct Dhcp<'a> {
    pub op: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ip,
    pub yiaddr: Ip,
    pub siaddr: Ip,
    pub giaddr: Ip,
    pub chaddr: Mac,
    /// Everything after the magic cookie.
    pub options: &'a [u8],
}

/// Parse a DHCP message (the UDP payload). `Truncated` under 240 bytes; `Malformed` for a bad cookie, an op other than
/// 1 or 2, or a hardware type other than Ethernet.
pub fn parse(p: &[u8]) -> Result<Dhcp<'_>, WireError> {
    if p.len() < FIXED {
        return Err(WireError::Truncated);
    }
    let op = p[0];
    if (op != OP_REQUEST && op != OP_REPLY) || p[1] != 1 || p[2] != 6 || p[236..240] != MAGIC {
        return Err(WireError::Malformed);
    }
    Ok(Dhcp {
        op,
        xid: be32(p, 4)?,
        secs: be16(p, 8)?,
        flags: be16(p, 10)?,
        ciaddr: arr(p, 12)?,
        yiaddr: arr(p, 16)?,
        siaddr: arr(p, 20)?,
        giaddr: arr(p, 24)?,
        chaddr: arr(p, 28)?,
        options: p.get(FIXED..).ok_or(WireError::Truncated)?,
    })
}

/// The options in order, as `(code, value)`. Pad bytes are skipped; the list ends at `END`, at the end of the buffer,
/// or at an option whose length runs past the end (that option is not returned).
pub fn options(mut o: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    core::iter::from_fn(move || loop {
        let (&code, rest) = o.split_first()?;
        match code {
            opt::PAD => {
                o = rest;
            }
            opt::END => {
                o = &[];
                return None;
            }
            _ => {
                let (&len, rest) = rest.split_first().or_else(|| {
                    o = &[];
                    None
                })?;
                let len = usize::from(len);
                if rest.len() < len {
                    o = &[];
                    return None;
                }
                let (val, rest) = rest.split_at(len);
                o = rest;
                return Some((code, val));
            }
        }
    })
}

/// What the client uses from an option list: the first occurrence of each, with the wrong-sized ones ignored.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Summary {
    pub msg_type: Option<u8>,
    pub server_id: Option<Ip>,
    pub mask: Option<Ip>,
    pub router: Option<Ip>,
    pub dns: Option<Ip>,
    pub lease: Option<u32>,
    pub t1: Option<u32>,
    pub t2: Option<u32>,
    pub requested: Option<Ip>,
}

fn ip4(v: &[u8]) -> Option<Ip> {
    v.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok())
}

fn u32be(v: &[u8]) -> Option<u32> {
    (v.len() == 4).then(|| u32::from_be_bytes([v[0], v[1], v[2], v[3]]))
}

impl Dhcp<'_> {
    pub fn summary(&self) -> Summary {
        let mut s = Summary::default();
        for (code, v) in options(self.options) {
            match code {
                opt::MSG_TYPE if v.len() == 1 => s.msg_type = s.msg_type.or(Some(v[0])),
                opt::SERVER_ID if v.len() == 4 => s.server_id = s.server_id.or(ip4(v)),
                opt::MASK if v.len() == 4 => s.mask = s.mask.or(ip4(v)),
                // router and DNS: the first address of the list (a list is a multiple of four bytes)
                opt::ROUTER if v.len() >= 4 && v.len() % 4 == 0 => s.router = s.router.or(ip4(v)),
                opt::DNS if v.len() >= 4 && v.len() % 4 == 0 => s.dns = s.dns.or(ip4(v)),
                opt::LEASE => s.lease = s.lease.or(u32be(v)),
                opt::T1 => s.t1 = s.t1.or(u32be(v)),
                opt::T2 => s.t2 = s.t2.or(u32be(v)),
                opt::REQUESTED_IP if v.len() == 4 => s.requested = s.requested.or(ip4(v)),
                _ => {}
            }
        }
        s
    }
}

/// The fixed fields of a message to build.
#[derive(Clone, Copy, Debug)]
pub struct Fixed {
    pub op: u8,
    pub xid: u32,
    pub secs: u16,
    pub flags: u16,
    pub ciaddr: Ip,
    pub yiaddr: Ip,
    pub siaddr: Ip,
    pub giaddr: Ip,
    pub chaddr: Mac,
}

/// Write the 240 bytes (header, zeroed sname and file, cookie) at the start of `buf`.
pub fn write_fixed(buf: &mut [u8], f: &Fixed) -> Result<usize, WireError> {
    let o = out(buf, 0, FIXED)?;
    o.fill(0);
    o[0] = f.op;
    o[1] = 1;
    o[2] = 6;
    o[4..8].copy_from_slice(&f.xid.to_be_bytes());
    o[8..10].copy_from_slice(&f.secs.to_be_bytes());
    o[10..12].copy_from_slice(&f.flags.to_be_bytes());
    o[12..16].copy_from_slice(&f.ciaddr);
    o[16..20].copy_from_slice(&f.yiaddr);
    o[20..24].copy_from_slice(&f.siaddr);
    o[24..28].copy_from_slice(&f.giaddr);
    o[28..34].copy_from_slice(&f.chaddr);
    o[236..240].copy_from_slice(&MAGIC);
    Ok(FIXED)
}

/// An option writer over the space after the fixed header.
pub struct Options<'a> {
    buf: &'a mut [u8],
    at: usize,
}

impl<'a> Options<'a> {
    /// `buf` is the whole message buffer; options start at 240.
    pub fn new(buf: &'a mut [u8]) -> Options<'a> {
        Options { buf, at: FIXED }
    }

    pub fn put(&mut self, code: u8, value: &[u8]) -> Result<(), WireError> {
        let len = u8::try_from(value.len()).map_err(|_| WireError::NoSpace)?;
        let o = out(self.buf, self.at, value.len() + 2)?;
        o[0] = code;
        o[1] = len;
        o[2..].copy_from_slice(value);
        self.at += value.len() + 2;
        Ok(())
    }

    /// Write `END` and return the message length.
    pub fn finish(self) -> Result<usize, WireError> {
        out(self.buf, self.at, 1)?[0] = opt::END;
        Ok(self.at + 1)
    }
}

/// Build a client DISCOVER or REQUEST (RFC 2131 4.4.1, 4.3.2) with the broadcast flag set as the caller says,
/// the client identifier (type 1 and the MAC), the parameter request list (mask, router, DNS, lease, T1, T2) and, for a
/// REQUEST, the requested address and the server identifier when given. Returns the length.
#[allow(clippy::too_many_arguments)]
pub fn write_client(
    buf: &mut [u8],
    kind: u8,
    xid: u32,
    mac: Mac,
    flags: u16,
    secs: u16,
    ciaddr: Ip,
    requested: Option<Ip>,
    server: Option<Ip>,
) -> Result<usize, WireError> {
    write_fixed(
        buf,
        &Fixed {
            op: OP_REQUEST,
            xid,
            secs,
            flags,
            ciaddr,
            yiaddr: [0; 4],
            siaddr: [0; 4],
            giaddr: [0; 4],
            chaddr: mac,
        },
    )?;
    let mut o = Options::new(buf);
    o.put(opt::MSG_TYPE, &[kind])?;
    let mut cid = [1u8; 7];
    cid[1..].copy_from_slice(&mac);
    o.put(opt::CLIENT_ID, &cid)?;
    if let Some(r) = requested {
        o.put(opt::REQUESTED_IP, &r)?;
    }
    if let Some(s) = server {
        o.put(opt::SERVER_ID, &s)?;
    }
    o.put(
        opt::PARAM_LIST,
        &[
            opt::MASK,
            opt::ROUTER,
            opt::DNS,
            opt::LEASE,
            opt::T1,
            opt::T2,
        ],
    )?;
    o.finish()
}
