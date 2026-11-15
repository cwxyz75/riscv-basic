//! The retransmission timer's estimator (RFC 6298, S9-SPEC 5.5), integer nanoseconds, exactly as the RFC states it.

pub const MS: u64 = 1_000_000;
pub const INITIAL_RTO: u64 = 1_000 * MS;
pub const MIN_RTO: u64 = 200 * MS;
pub const MAX_RTO: u64 = 60_000 * MS;
/// The clock granularity G of the RFC.
pub const GRANULARITY: u64 = MS;

#[derive(Clone, Copy, Debug)]
pub struct Rto {
    srtt: u64,
    rttvar: u64,
    have_sample: bool,
    /// The current timeout before any backoff doubling is applied by the caller's `backoff` count.
    rto: u64,
}

impl Default for Rto {
    fn default() -> Rto {
        Rto::new()
    }
}

impl Rto {
    pub fn new() -> Rto {
        Rto {
            srtt: 0,
            rttvar: 0,
            have_sample: false,
            rto: INITIAL_RTO,
        }
    }

    pub fn rto(&self) -> u64 {
        self.rto
    }
    pub fn srtt(&self) -> Option<u64> {
        self.have_sample.then_some(self.srtt)
    }
    pub fn rttvar(&self) -> u64 {
        self.rttvar
    }

    /// A round-trip measurement `r` of a segment sent once (Karn: the caller never passes a retransmitted one).
    pub fn sample(&mut self, r: u64) {
        if !self.have_sample {
            self.srtt = r;
            self.rttvar = r / 2;
            self.have_sample = true;
        } else {
            // RTTVAR = (1 - 1/4) RTTVAR + 1/4 |SRTT - R|; SRTT = (1 - 1/8) SRTT + 1/8 R   (RTTVAR first, per the RFC)
            let err = self.srtt.abs_diff(r);
            self.rttvar = self.rttvar - self.rttvar / 4 + err / 4;
            self.srtt = self.srtt - self.srtt / 8 + r / 8;
        }
        let k = (4 * self.rttvar).max(GRANULARITY);
        self.rto = (self.srtt + k).clamp(MIN_RTO, MAX_RTO);
    }

    /// The timeout after `n` consecutive expirations (doubling, capped).
    pub fn backed_off(&self, n: u32) -> u64 {
        self.rto.saturating_mul(1u64 << n.min(16)).min(MAX_RTO)
    }

    /// After a SYN or SYN-ACK was retransmitted the first data timeout is 3 s (RFC 6298 5.7).
    pub fn after_syn_retransmit(&mut self) {
        if !self.have_sample {
            self.rto = 3_000 * MS;
        }
    }
}
