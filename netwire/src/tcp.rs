//! TCP segments over IPv4 (S9-SPEC 5.1): a view over the IP payload and a builder. Options understood: end (0), no-op
//! (1), MSS (2, length 4, only on a SYN); window scale, SACK-permitted, SACK and timestamps are recognised by number only
//! (so a counter can say a peer offered them) and every other kind is skipped by its length. A length of 0 or 1, or one
//! that runs past the header, ends the option parse: the segment is still processed without the rest.
use crate::checksum;
use crate::{be16, be32, out, Ip, WireError};

pub const HEADER: usize = 20;
/// The longest header (data offset 15).
pub const MAX_HEADER: usize = 60;

pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
pub const URG: u8 = 0x20;

pub const OPT_END: u8 = 0;
pub const OPT_NOP: u8 = 1;
pub const OPT_MSS: u8 = 2;
pub const OPT_WINDOW_SCALE: u8 = 3;
pub const OPT_SACK_PERMITTED: u8 = 4;
pub const OPT_SACK: u8 = 5;
pub const OPT_TIMESTAMPS: u8 = 8;

/// What the options said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Options {
    /// The MSS option, on a SYN only.
    pub mss: Option<u16>,
    pub window_scale_offered: bool,
    pub sack_permitted_offered: bool,
    pub sack_seen: bool,
    pub timestamps_offered: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Tcp<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    /// The flag byte (FIN, SYN, RST, PSH, ACK, URG; ECE and CWR are in the bits above and ignored).
    pub flags: u8,
    pub window: u16,
    pub urgent: u16,
    /// The option bytes as they were on the wire (for rebuilding and goldens).
    pub options_raw: &'a [u8],
    pub options: Options,
    pub payload: &'a [u8],
}

impl Tcp<'_> {
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    /// Sequence space the segment occupies: the data, plus one each for SYN and FIN.
    pub fn seg_len(&self) -> u32 {
        (self.payload.len() as u32)
            .wrapping_add(u32::from(self.has(SYN)))
            .wrapping_add(u32::from(self.has(FIN)))
    }
}

/// Parse the options in `raw` (a SYN allows the MSS option). Total: any bytes give some answer.
pub fn parse_options(raw: &[u8], syn: bool) -> Options {
    let mut o = Options::default();
    let mut i = 0usize;
    while let Some(&kind) = raw.get(i) {
        match kind {
            OPT_END => break,
            OPT_NOP => {
                i += 1;
                continue;
            }
            _ => {}
        }
        let len = match raw.get(i + 1) {
            Some(&l) if l >= 2 && i + usize::from(l) <= raw.len() => usize::from(l),
            _ => break, // length 0, 1, missing, or past the header: stop here
        };
        match kind {
            OPT_MSS if syn && len == 4 => {
                o.mss = Some(u16::from_be_bytes([raw[i + 2], raw[i + 3]]));
            }
            OPT_WINDOW_SCALE => o.window_scale_offered = true,
            OPT_SACK_PERMITTED => o.sack_permitted_offered = true,
            OPT_SACK => o.sack_seen = true,
            OPT_TIMESTAMPS => o.timestamps_offered = true,
            _ => {}
        }
        i += len;
    }
    o
}

/// Parse a segment (the IP payload, exactly as long as the IP length says) sent from `src` to `dst`. `Truncated` under 20
/// bytes or when the header runs past the bytes present; `Malformed` for a data offset below 5; `BadChecksum` when it does
/// not verify (the checksum is never optional in TCP).
pub fn parse<'a>(d: &'a [u8], src: Ip, dst: Ip) -> Result<Tcp<'a>, WireError> {
    if d.len() < HEADER {
        return Err(WireError::Truncated);
    }
    let off = usize::from(d[12] >> 4) * 4;
    if off < HEADER {
        return Err(WireError::Malformed);
    }
    if off > d.len() {
        return Err(WireError::Truncated);
    }
    let len = u16::try_from(d.len()).map_err(|_| WireError::BadLength)?;
    if !checksum::verifies(d, checksum::pseudo(src, dst, 6, len)) {
        return Err(WireError::BadChecksum);
    }
    let flags = d[13];
    let options_raw = &d[HEADER..off];
    Ok(Tcp {
        src_port: be16(d, 0)?,
        dst_port: be16(d, 2)?,
        seq: be32(d, 4)?,
        ack: be32(d, 8)?,
        flags,
        window: be16(d, 14)?,
        urgent: be16(d, 18)?,
        options_raw,
        options: parse_options(options_raw, flags & SYN != 0),
        payload: &d[off..],
    })
}

/// The fields of a segment to write.
#[derive(Clone, Copy, Debug, Default)]
pub struct Header {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
}

/// The MSS option bytes (kind 2, length 4).
pub fn mss_option(mss: u16) -> [u8; 4] {
    let b = mss.to_be_bytes();
    [OPT_MSS, 4, b[0], b[1]]
}

/// Write a segment with its checksum. `options` must be a multiple of four bytes and at most 40. Returns its length.
pub fn write(
    buf: &mut [u8],
    src: Ip,
    dst: Ip,
    h: &Header,
    options: &[u8],
    payload: &[u8],
) -> Result<usize, WireError> {
    if !options.len().is_multiple_of(4) || options.len() > MAX_HEADER - HEADER {
        return Err(WireError::Malformed);
    }
    let hlen = HEADER + options.len();
    let n = hlen.checked_add(payload.len()).ok_or(WireError::NoSpace)?;
    let n16 = u16::try_from(n).map_err(|_| WireError::NoSpace)?;
    let o = out(buf, 0, n)?;
    o[0..2].copy_from_slice(&h.src_port.to_be_bytes());
    o[2..4].copy_from_slice(&h.dst_port.to_be_bytes());
    o[4..8].copy_from_slice(&h.seq.to_be_bytes());
    o[8..12].copy_from_slice(&h.ack.to_be_bytes());
    o[12] = ((hlen / 4) as u8) << 4;
    o[13] = h.flags;
    o[14..16].copy_from_slice(&h.window.to_be_bytes());
    o[16..20].copy_from_slice(&[0, 0, 0, 0]);
    o[HEADER..hlen].copy_from_slice(options);
    o[hlen..].copy_from_slice(payload);
    let c = !checksum::fold(checksum::sum16(o, checksum::pseudo(src, dst, 6, n16)));
    o[16..18].copy_from_slice(&c.to_be_bytes());
    Ok(n)
}
