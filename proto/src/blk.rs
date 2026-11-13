//! The block protocol (S6-SPEC section 5): request/response over a channel, one transaction id per
//! call. All integers are little-endian.
//!
//! Request: `txid u32 | op u16 | flags u16 (0) | sector u64 | count u32 | data`.
//! Reply:   `txid u32 | status i32 | body`.
//! `Info` has no data and its reply body is `sectors u64 | sector_size u32 | flags u32 | max_io u32`
//! (flags bit 0: read only). `Read`'s reply body is `count * 512` bytes. `Write` carries
//! `count * 512` bytes of data. `Flush` has neither.

use crate::{rd_u16, rd_u32, rd_u64};
use alloc::vec::Vec;

pub const SECTOR: usize = 512;
/// Most sectors one `Read` or `Write` moves (8 KiB).
pub const MAX_IO_SECTORS: u32 = 16;
pub const REQ_HEADER: usize = 20;
pub const REPLY_HEADER: usize = 8;
pub const INFO_BODY: usize = 20;

pub const OP_INFO: u16 = 1;
pub const OP_READ: u16 = 2;
pub const OP_WRITE: u16 = 3;
pub const OP_FLUSH: u16 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Ok = 0,
    BadRange = 1,
    TooBig = 2,
    ReadOnly = 3,
    IoError = 4,
    Timeout = 5,
    BadRequest = 6,
}

impl Status {
    pub fn from_i32(v: i32) -> Option<Status> {
        Some(match v {
            0 => Status::Ok,
            1 => Status::BadRange,
            2 => Status::TooBig,
            3 => Status::ReadOnly,
            4 => Status::IoError,
            5 => Status::Timeout,
            6 => Status::BadRequest,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op<'a> {
    Info,
    Read {
        sector: u64,
        count: u32,
    },
    Write {
        sector: u64,
        count: u32,
        data: &'a [u8],
    },
    Flush,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    pub txid: u32,
    pub op: Op<'a>,
}

/// What `Info` reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub sectors: u64,
    pub sector_size: u32,
    pub read_only: bool,
    pub max_io: u32,
}

/// A malformed request: the transaction id (when the header was long enough to carry one) so the
/// server can still answer BAD_REQUEST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Malformed {
    pub txid: Option<u32>,
}

impl Request<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let (code, sector, count, data): (u16, u64, u32, &[u8]) = match &self.op {
            Op::Info => (OP_INFO, 0, 0, &[]),
            Op::Read { sector, count } => (OP_READ, *sector, *count, &[]),
            Op::Write {
                sector,
                count,
                data,
            } => (OP_WRITE, *sector, *count, data),
            Op::Flush => (OP_FLUSH, 0, 0, &[]),
        };
        let mut out = Vec::with_capacity(REQ_HEADER + data.len());
        out.extend_from_slice(&self.txid.to_le_bytes());
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&sector.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(data);
        out
    }
}

/// Decode a request. Structure only: a count of zero or past `MAX_IO_SECTORS`, or a range past
/// the end of the disk, is for [`check`] to judge with a proper status.
pub fn decode_request(b: &[u8]) -> Result<Request<'_>, Malformed> {
    let txid = rd_u32(b, 0);
    let bad = Malformed { txid };
    if b.len() < REQ_HEADER {
        return Err(bad);
    }
    let (Some(txid), Some(op), Some(flags), Some(sector), Some(count)) = (
        rd_u32(b, 0),
        rd_u16(b, 4),
        rd_u16(b, 6),
        rd_u64(b, 8),
        rd_u32(b, 16),
    ) else {
        return Err(bad);
    };
    if flags != 0 {
        return Err(bad);
    }
    let data = &b[REQ_HEADER..];
    let op = match op {
        OP_INFO if data.is_empty() && sector == 0 && count == 0 => Op::Info,
        OP_FLUSH if data.is_empty() && sector == 0 && count == 0 => Op::Flush,
        OP_READ if data.is_empty() => Op::Read { sector, count },
        OP_WRITE if data.len() as u64 == u64::from(count) * SECTOR as u64 => Op::Write {
            sector,
            count,
            data,
        },
        _ => return Err(bad),
    };
    Ok(Request { txid, op })
}

/// The status an I/O of `count` sectors at `sector` gets on a disk of `sectors` sectors.
pub fn check(sectors: u64, read_only: bool, writing: bool, sector: u64, count: u32) -> Status {
    if count == 0 {
        return Status::BadRequest;
    }
    if count > MAX_IO_SECTORS {
        return Status::TooBig;
    }
    if writing && read_only {
        return Status::ReadOnly;
    }
    match sector.checked_add(u64::from(count)) {
        Some(end) if end <= sectors => Status::Ok,
        _ => Status::BadRange,
    }
}

pub fn encode_reply(txid: u32, status: Status, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(REPLY_HEADER + body.len());
    out.extend_from_slice(&txid.to_le_bytes());
    out.extend_from_slice(&(status as i32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

pub fn encode_info(i: &Info) -> Vec<u8> {
    let mut b = Vec::with_capacity(INFO_BODY);
    b.extend_from_slice(&i.sectors.to_le_bytes());
    b.extend_from_slice(&i.sector_size.to_le_bytes());
    b.extend_from_slice(&u32::from(i.read_only).to_le_bytes());
    b.extend_from_slice(&i.max_io.to_le_bytes());
    b
}

/// Split a reply into `(txid, status, body)`.
pub fn decode_reply(b: &[u8]) -> Option<(u32, Status, &[u8])> {
    let status = Status::from_i32(rd_u32(b, 4)? as i32)?;
    Some((rd_u32(b, 0)?, status, &b[REPLY_HEADER..]))
}

pub fn decode_info(body: &[u8]) -> Option<Info> {
    if body.len() != INFO_BODY {
        return None;
    }
    Some(Info {
        sectors: rd_u64(body, 0)?,
        sector_size: rd_u32(body, 8)?,
        read_only: rd_u32(body, 12)? & 1 != 0,
        max_io: rd_u32(body, 16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn golden_encodings() {
        let r = Request {
            txid: 7,
            op: Op::Read {
                sector: 0x0102,
                count: 3,
            },
        };
        assert_eq!(
            r.encode(),
            [7, 0, 0, 0, 2, 0, 0, 0, 2, 1, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0]
        );
        let data = vec![0xabu8; 1024];
        let w = Request {
            txid: 9,
            op: Op::Write {
                sector: 5,
                count: 2,
                data: &data,
            },
        };
        let e = w.encode();
        assert_eq!(
            &e[..20],
            &[9, 0, 0, 0, 3, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0]
        );
        assert_eq!(e.len(), 20 + 1024);
        assert_eq!(decode_request(&e), Ok(w));
        assert_eq!(
            encode_reply(7, Status::BadRange, &[]),
            [7, 0, 0, 0, 1, 0, 0, 0]
        );
        let info = Info {
            sectors: 32768,
            sector_size: 512,
            read_only: false,
            max_io: 16,
        };
        assert_eq!(
            encode_info(&info),
            [0, 0x80, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 16, 0, 0, 0]
        );
        assert_eq!(decode_info(&encode_info(&info)), Some(info));
    }

    #[test]
    fn round_trips() {
        for op in [
            Op::Info,
            Op::Flush,
            Op::Read {
                sector: u64::MAX,
                count: u32::MAX,
            },
        ] {
            let r = Request { txid: 1, op };
            // Info and Flush must have zero sector and count; the others anything
            assert_eq!(decode_request(&r.encode()), Ok(r));
        }
    }

    #[test]
    fn malformed_requests_are_refused_with_the_txid_when_there_is_one() {
        assert_eq!(decode_request(&[]), Err(Malformed { txid: None }));
        assert_eq!(decode_request(&[1, 2, 3]), Err(Malformed { txid: None }));
        assert_eq!(
            decode_request(&[5, 0, 0, 0, 9]),
            Err(Malformed { txid: Some(5) })
        );
        let mut ok = Request {
            txid: 4,
            op: Op::Info,
        }
        .encode();
        ok[6] = 1; // flags
        assert_eq!(decode_request(&ok), Err(Malformed { txid: Some(4) }));
        let mut unknown = Request {
            txid: 4,
            op: Op::Info,
        }
        .encode();
        unknown[4] = 99;
        assert_eq!(decode_request(&unknown), Err(Malformed { txid: Some(4) }));
        // Info with a nonzero sector, or trailing data, is not Info
        let mut info = Request {
            txid: 4,
            op: Op::Info,
        }
        .encode();
        info[8] = 1;
        assert!(decode_request(&info).is_err());
        let mut read = Request {
            txid: 4,
            op: Op::Read {
                sector: 0,
                count: 1,
            },
        }
        .encode();
        read.push(0);
        assert!(decode_request(&read).is_err(), "a read carries no data");
        // a write whose data length is not count * 512
        let mut w = Request {
            txid: 4,
            op: Op::Write {
                sector: 0,
                count: 1,
                data: &[0; 512],
            },
        }
        .encode();
        w.pop();
        assert!(decode_request(&w).is_err());
    }

    #[test]
    fn check_judges_ranges_and_sizes() {
        assert_eq!(check(100, false, false, 0, 0), Status::BadRequest);
        assert_eq!(check(100, false, false, 0, 17), Status::TooBig);
        assert_eq!(check(100, false, false, 0, 16), Status::Ok);
        assert_eq!(check(100, false, false, 84, 16), Status::Ok);
        assert_eq!(check(100, false, false, 85, 16), Status::BadRange);
        assert_eq!(check(100, false, false, 100, 1), Status::BadRange);
        assert_eq!(
            check(100, false, false, u64::MAX, 2),
            Status::BadRange,
            "no wraparound"
        );
        assert_eq!(check(100, true, true, 0, 1), Status::ReadOnly);
        assert_eq!(check(100, true, false, 0, 1), Status::Ok);
        // too big is reported before read-only and range, count 0 before everything
        assert_eq!(check(100, true, true, 1000, 99), Status::TooBig);
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut x = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..20000 {
            let len = (next() % 64) as usize;
            let b: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = decode_request(&b);
            let _ = decode_reply(&b);
            let _ = decode_info(&b);
        }
    }
}
