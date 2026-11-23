//! The data page (read-only for programs, written by the kernel): eight little-endian `u64` words at the start of the library's second segment.

/// The page's size.
pub const SIZE: usize = 4096;

/// Word 0: the version of the layout and the call set (`ABI`).
pub const ABI_VERSION: usize = 0;
/// Word 1: `ns = (ticks * mult) >> shift` for the `time` counter.
pub const TIME_MULT: usize = 1;
/// Word 2: the shift of the line above.
pub const TIME_SHIFT: usize = 2;
/// Word 3: the page size.
pub const PAGE_SIZE: usize = 3;
/// Word 4: the cache-block size of `cbo.clean/flush/inval`.
pub const CBOM_BLOCK: usize = 4;
/// Word 5: the cache-block size of `cbo.zero`.
pub const CBOZ_BLOCK: usize = 5;
/// Word 6: the harts that are online (changes as harts go offline and return).
pub const HARTS_ONLINE: usize = 6;
/// Word 7: flags (none yet).
pub const FLAGS: usize = 7;
/// Word 8: the `time` counter's value at the clock's zero (boot): `clock_get` is `((time - TIME_BASE) * TIME_MULT) >> TIME_SHIFT`.
pub const TIME_BASE: usize = 8;
/// How many words are defined.
pub const WORDS: usize = 9;

/// The version the kernel writes in word 0 (AB9 gives the numbering its rules).
pub const ABI: u64 = 1;

/// What the kernel puts in the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Page {
    pub abi_version: u64,
    pub time_mult: u64,
    pub time_shift: u64,
    pub page_size: u64,
    pub cbom_block: u64,
    pub cboz_block: u64,
    pub harts_online: u64,
    pub flags: u64,
    pub time_base: u64,
}

impl Page {
    /// The page's first `WORDS * 8` bytes (the rest is zero).
    pub fn to_bytes(&self) -> [u8; WORDS * 8] {
        let words = [
            self.abi_version,
            self.time_mult,
            self.time_shift,
            self.page_size,
            self.cbom_block,
            self.cboz_block,
            self.harts_online,
            self.flags,
            self.time_base,
        ];
        let mut out = [0u8; WORDS * 8];
        for (i, w) in words.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// The inverse of [`Page::to_bytes`].
    pub fn from_bytes(b: &[u8]) -> Option<Page> {
        let w = |i: usize| -> Option<u64> {
            let s = b.get(i * 8..i * 8 + 8)?;
            Some(u64::from_le_bytes(s.try_into().ok()?))
        };
        Some(Page {
            abi_version: w(ABI_VERSION)?,
            time_mult: w(TIME_MULT)?,
            time_shift: w(TIME_SHIFT)?,
            page_size: w(PAGE_SIZE)?,
            cbom_block: w(CBOM_BLOCK)?,
            cboz_block: w(CBOZ_BLOCK)?,
            harts_online: w(HARTS_ONLINE)?,
            flags: w(FLAGS)?,
            time_base: w(TIME_BASE)?,
        })
    }
}
