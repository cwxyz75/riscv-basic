//! Graphics: a 640x480 XRGB framebuffer through QEMU's `ramfb` device.
//!
//! `ramfb` is configured through `fw_cfg`, QEMU's firmware-configuration
//! device (memory-mapped at 0x1010_0000 on the `virt` board). We find the file
//! `etc/ramfb` in its directory and write a small config block to it with a DMA
//! transfer: where the framebuffer is in RAM, its pixel format and its size.
//! After that QEMU reads the pixels straight out of our memory.
//!
//! All `fw_cfg` fields are big-endian. Start QEMU with `-device ramfb` or the
//! device is absent and `init` reports an error.

use core::ptr::{addr_of, addr_of_mut, read_volatile, write_volatile};
use turtle::{rgb, Canvas};

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
/// DRM fourcc `XR24`: 32 bits per pixel, `0x00RRGGBB`.
const FOURCC_XRGB8888: u32 = 0x3432_5258;

const FW_CFG_BASE: usize = 0x1010_0000;
const FW_CFG_DATA: usize = FW_CFG_BASE; // read the selected item, byte by byte
const FW_CFG_SELECTOR: usize = FW_CFG_BASE + 0x8; // 16-bit, big-endian
const FW_CFG_DMA: usize = FW_CFG_BASE + 0x10; // 64-bit address, big-endian
const FW_CFG_FILE_DIR: u16 = 0x19;
const DMA_ERROR: u32 = 1 << 0;
const DMA_SELECT: u32 = 1 << 3;
const DMA_WRITE: u32 = 1 << 4;

/// The pixels. 4 KiB aligned; lives in `.bss`, so it costs nothing in the image.
// The array is only touched through raw pointers (the slice `init` returns).
#[allow(dead_code)]
#[repr(align(4096))]
struct Framebuffer([u32; WIDTH * HEIGHT]);
static mut FRAMEBUFFER: Framebuffer = Framebuffer([0; WIDTH * HEIGHT]);

/// The palette index of every pixel, so the turtle can read the picture back
/// (flood fill, sprite toggling). The framebuffer itself only holds colours.
static mut INDICES: [u8; WIDTH * HEIGHT] = [0; WIDTH * HEIGHT];

/// A turtle [`Canvas`] over the framebuffer: palette indices are kept in
/// `INDICES` and the matching colour is written to the screen.
pub struct FbCanvas {
    pixels: &'static mut [u32],
    indices: &'static mut [u8],
}

impl FbCanvas {
    /// Wrap the framebuffer `pixels` returned by [`init`]. Call once.
    pub fn new(pixels: &'static mut [u32]) -> Self {
        // SAFETY: `INDICES` is private and, after this, reachable only through
        // the slice held by the one `FbCanvas`; it is created once per boot.
        let indices: &'static mut [u8] = unsafe {
            core::slice::from_raw_parts_mut(addr_of_mut!(INDICES).cast::<u8>(), WIDTH * HEIGHT)
        };
        FbCanvas { pixels, indices }
    }
}

impl Canvas for FbCanvas {
    fn width(&self) -> i32 {
        WIDTH as i32
    }

    fn height(&self) -> i32 {
        HEIGHT as i32
    }

    fn pixel(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= WIDTH as i32 || y >= HEIGHT as i32 {
            return 0;
        }
        self.indices[y as usize * WIDTH + x as usize]
    }

    fn set_pixel(&mut self, x: i32, y: i32, color: u8) {
        if x >= 0 && y >= 0 && x < WIDTH as i32 && y < HEIGHT as i32 {
            let i = y as usize * WIDTH + x as usize;
            self.indices[i] = color & 0x0F;
            self.pixels[i] = rgb(color);
        }
    }

    fn clear(&mut self, color: u8) {
        self.indices.fill(color & 0x0F);
        self.pixels.fill(rgb(color));
    }
}

/// The `etc/ramfb` configuration block (all fields big-endian). It must be
/// exactly 28 bytes, as the device's file is, so it is packed (plain `repr(C)`
/// would pad it to 32 and the transfer would be rejected).
#[repr(C, packed)]
struct RamfbConfig {
    address: u64,
    fourcc: u32,
    flags: u32,
    width: u32,
    height: u32,
    stride: u32,
}

/// An `fw_cfg` DMA descriptor (all fields big-endian).
#[repr(C)]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

fn select(key: u16) {
    // SAFETY: the `fw_cfg` selector register is a 16-bit big-endian MMIO
    // register at this fixed address on QEMU's virt machine.
    unsafe { write_volatile(FW_CFG_SELECTOR as *mut u16, key.to_be()) }
}

fn read_byte() -> u8 {
    // SAFETY: the `fw_cfg` data register returns the next byte of the selected
    // item on each read; it is a byte-wide MMIO register.
    unsafe { read_volatile(FW_CFG_DATA as *const u8) }
}

fn read_be16() -> u16 {
    u16::from_be_bytes([read_byte(), read_byte()])
}

fn read_be32() -> u32 {
    u32::from_be_bytes([read_byte(), read_byte(), read_byte(), read_byte()])
}

/// Find the selector for the `fw_cfg` file `name`, if the machine has it.
fn find_file(name: &str) -> Option<u16> {
    select(FW_CFG_FILE_DIR);
    let count = read_be32();
    for _ in 0..count {
        let _size = read_be32();
        let selector = read_be16();
        let _reserved = read_be16();
        let mut entry = [0u8; 56];
        for byte in entry.iter_mut() {
            *byte = read_byte();
        }
        let len = entry.iter().position(|&b| b == 0).unwrap_or(entry.len());
        if &entry[..len] == name.as_bytes() {
            return Some(selector);
        }
    }
    None
}

/// Write `data` to the `fw_cfg` item `selector` with a DMA transfer.
fn dma_write(selector: u16, data: *const u8, len: usize) -> Result<(), &'static str> {
    let mut access = DmaAccess {
        control: ((u32::from(selector) << 16) | DMA_SELECT | DMA_WRITE).to_be(),
        length: (len as u32).to_be(),
        address: (data as u64).to_be(),
    };
    let descriptor = addr_of_mut!(access) as u64;
    // SAFETY: the DMA register takes the physical address of the descriptor as
    // a big-endian 64-bit value; writing the low half starts the transfer. The
    // descriptor and the data live in memory that is identity-mapped (paging is
    // off) and stay alive until the transfer is seen to finish below.
    unsafe {
        write_volatile(FW_CFG_DMA as *mut u32, ((descriptor >> 32) as u32).to_be());
        write_volatile((FW_CFG_DMA + 4) as *mut u32, (descriptor as u32).to_be());
        loop {
            // QEMU clears `control` when done, or leaves only the error bit.
            let control = u32::from_be(read_volatile(addr_of!(access.control)));
            if control == 0 {
                return Ok(());
            }
            if control == DMA_ERROR {
                return Err("fw_cfg DMA transfer failed");
            }
        }
    }
}

/// Turn the framebuffer on and return its pixels, or say why it cannot be.
pub fn init() -> Result<&'static mut [u32], &'static str> {
    let selector =
        find_file("etc/ramfb").ok_or("no ramfb device (start QEMU with -device ramfb)")?;
    // SAFETY: `FRAMEBUFFER` is private and, after this point, reachable only
    // through the slice returned here; `init` is called at most once per boot.
    let pixels: &'static mut [u32] = unsafe {
        core::slice::from_raw_parts_mut(addr_of_mut!(FRAMEBUFFER).cast::<u32>(), WIDTH * HEIGHT)
    };
    let config = RamfbConfig {
        address: (pixels.as_ptr() as u64).to_be(),
        fourcc: FOURCC_XRGB8888.to_be(),
        flags: 0,
        width: (WIDTH as u32).to_be(),
        height: (HEIGHT as u32).to_be(),
        stride: ((WIDTH * 4) as u32).to_be(),
    };
    dma_write(
        selector,
        addr_of!(config).cast::<u8>(),
        core::mem::size_of::<RamfbConfig>(),
    )?;
    Ok(pixels)
}
