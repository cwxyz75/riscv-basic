//! The 16-colour palette (UCBLogo's), as `0x00RRGGBB`.

const PALETTE: [u32; 16] = [
    0x000000, // 0 black
    0x0000ff, // 1 blue
    0x00ff00, // 2 green
    0x00ffff, // 3 cyan
    0xff0000, // 4 red
    0xff00ff, // 5 magenta
    0xffff00, // 6 yellow
    0xffffff, // 7 white
    0x9b603b, // 8 brown
    0xc58812, // 9 tan
    0x64a240, // 10 forest
    0x78bbbb, // 11 aqua
    0xff9577, // 12 salmon
    0x9071d0, // 13 purple
    0xffa300, // 14 orange
    0xb7b7b7, // 15 grey
];

/// The colour of palette entry `index` (only the low four bits count).
pub fn rgb(index: u8) -> u32 {
    PALETTE[usize::from(index & 0x0F)]
}
