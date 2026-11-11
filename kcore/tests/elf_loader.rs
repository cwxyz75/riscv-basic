//! The ELF parser: hand-built images for every accept and reject rule, and a mutation
//! fuzz showing it never panics and only ever returns images that are safe to map.
mod common;
use abi::user::{BASE, TOP};
use common::{Rng, SEEDS};
use kcore::elf::*;
use kcore::pagetable::Perms;

const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

struct Ph {
    kind: u32,
    flags: u32,
    offset: u64,
    vaddr: u64,
    filesz: u64,
    memsz: u64,
}

fn load(flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64) -> Ph {
    Ph {
        kind: 1,
        flags,
        offset,
        vaddr,
        filesz,
        memsz,
    }
}

/// A minimal ELF64: header, program headers right after it, then `body` bytes.
fn build(entry: u64, phs: &[Ph], body: usize) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2; // 64-bit
    b[5] = 1; // little endian
    b[6] = 1;
    b[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    b[18..20].copy_from_slice(&243u16.to_le_bytes()); // EM_RISCV
    b[20..24].copy_from_slice(&1u32.to_le_bytes());
    b[24..32].copy_from_slice(&entry.to_le_bytes());
    b[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
    b[52..54].copy_from_slice(&64u16.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&(phs.len() as u16).to_le_bytes());
    for p in phs {
        let mut h = vec![0u8; 56];
        h[0..4].copy_from_slice(&p.kind.to_le_bytes());
        h[4..8].copy_from_slice(&p.flags.to_le_bytes());
        h[8..16].copy_from_slice(&p.offset.to_le_bytes());
        h[16..24].copy_from_slice(&p.vaddr.to_le_bytes());
        h[24..32].copy_from_slice(&p.vaddr.to_le_bytes());
        h[32..40].copy_from_slice(&p.filesz.to_le_bytes());
        h[40..48].copy_from_slice(&p.memsz.to_le_bytes());
        h[48..56].copy_from_slice(&4096u64.to_le_bytes());
        b.extend(h);
    }
    b.resize(b.len() + body, 0xaa);
    b
}

/// A good program: text at 0x10000 (file offset 0x1000), data+bss at 0x12000.
fn good() -> Vec<u8> {
    let phs = [
        load(PF_R | PF_X, 0x1000, 0x1_0000, 0x800, 0x800),
        load(PF_R | PF_W, 0x2000, 0x1_2000, 0x100, 0x3000),
    ];
    let mut b = build(0x1_0010, &phs, 0x2200);
    // make sure the file is long enough for both segments
    b.resize(0x2100, 0xaa);
    b
}

#[test]
fn a_good_image_parses_to_exactly_these_segments() {
    let img = parse(&good()).unwrap();
    assert_eq!(img.entry, 0x1_0010);
    assert_eq!(
        img.segments,
        vec![
            Segment {
                vaddr: 0x1_0000,
                memsz: 0x800,
                filesz: 0x800,
                offset: 0x1000,
                perms: Perms {
                    read: true,
                    write: false,
                    exec: true,
                    user: true
                }
            },
            Segment {
                vaddr: 0x1_2000,
                memsz: 0x3000,
                filesz: 0x100,
                offset: 0x2000,
                perms: Perms {
                    read: true,
                    write: true,
                    exec: false,
                    user: true
                }
            },
        ]
    );
    assert_eq!(img.segments[0].pages(), (0x10, 0x11));
    assert_eq!(
        img.segments[1].pages(),
        (0x12, 0x15),
        "0x3000 bytes at a page boundary: three pages"
    );
}

fn expect(bytes: Vec<u8>, want: ElfError) {
    assert_eq!(parse(&bytes), Err(want));
}

#[test]
fn every_rule_has_a_rejection() {
    let ok = good();
    expect(ok[..63].to_vec(), ElfError::TooShort);
    let mut b = ok.clone();
    b[0] = 0;
    expect(b, ElfError::BadMagic);
    let mut b = ok.clone();
    b[4] = 1;
    expect(b, ElfError::NotElf64);
    let mut b = ok.clone();
    b[5] = 2;
    expect(b, ElfError::NotLittleEndian);
    let mut b = ok.clone();
    b[16] = 3; // ET_DYN
    expect(b, ElfError::BadType);
    let mut b = ok.clone();
    b[18] = 62; // x86-64
    expect(b, ElfError::BadMachine);
    let mut b = ok.clone();
    b[20] = 2;
    expect(b, ElfError::BadVersion);
    let mut b = ok.clone();
    b[54] = 32; // phentsize
    expect(b, ElfError::BadProgramHeaders);
    let mut b = ok.clone();
    b[56] = 17; // 17 program headers
    expect(b, ElfError::TooManySegments);
    let mut b = ok.clone();
    b[32..40].copy_from_slice(&u64::MAX.to_le_bytes()); // phoff overflows
    expect(b, ElfError::BadProgramHeaders);
    let mut b = ok.clone();
    b[32..40].copy_from_slice(&(ok.len() as u64 - 10).to_le_bytes()); // table runs off the end
    expect(b, ElfError::BadProgramHeaders);
    // segments
    let img = |phs: &[Ph], entry| {
        let mut b = build(entry, phs, 0x4000);
        b.resize(0x4000, 0);
        b
    };
    let text = || load(PF_R | PF_X, 0x1000, 0x1_0000, 0x100, 0x100);
    expect(
        img(
            &[load(PF_R | PF_W | PF_X, 0x1000, 0x1_0000, 0x100, 0x100)],
            0x1_0000,
        ),
        ElfError::WriteExec,
    );
    expect(
        img(
            &[load(PF_W | PF_X, 0x1000, 0x1_0000, 0x100, 0x100)],
            0x1_0000,
        ),
        ElfError::WriteExec,
    );
    expect(
        img(&[load(PF_R, 0x1000, 0x1_0000, 0x200, 0x100)], 0x1_0000),
        ElfError::BadSegmentSize,
    );
    expect(
        img(
            &[load(PF_R | PF_X, 0x1000, BASE - 0x1000, 0x10, 0x10)],
            0x10,
        ),
        ElfError::SegmentOutsideUser,
    );
    expect(
        img(&[load(PF_R | PF_X, 0x1000, 0, 0x10, 0x10)], 0),
        ElfError::SegmentOutsideUser,
    );
    expect(
        img(
            &[load(PF_R | PF_X, 0x1000, TOP - 0x800, 0x10, 0x1000)],
            TOP - 0x800,
        ),
        ElfError::SegmentOutsideUser,
    );
    expect(
        img(
            &[load(PF_R | PF_X, 0x1000, u64::MAX - 8, 0x10, 0x100)],
            0x10,
        ),
        ElfError::SegmentOutsideUser,
    );
    expect(
        img(
            &[load(PF_R | PF_X, 0x1000, 0xffff_ffff_8020_0000, 0x10, 0x10)],
            0xffff_ffff_8020_0000,
        ),
        ElfError::SegmentOutsideUser,
    );
    expect(
        img(
            &[load(PF_R | PF_X, 0x3f00, 0x1_0000, 0x200, 0x200)],
            0x1_0000,
        ),
        ElfError::SegmentOutsideFile,
    );
    expect(
        img(
            &[load(PF_R | PF_X, u64::MAX - 4, 0x1_0000, 0x10, 0x10)],
            0x1_0000,
        ),
        ElfError::SegmentOutsideFile,
    );
    expect(
        img(&[load(PF_R | PF_X, 0x1010, 0x1_0000, 0x10, 0x10)], 0x1_0000),
        ElfError::SegmentMisaligned,
    );
    expect(
        img(&[load(PF_R | PF_X, 0, 0, 0, 0)], 0x1_0000),
        ElfError::NoLoadSegments,
    );
    expect(img(&[], 0x1_0000), ElfError::NoLoadSegments);
    expect(
        img(
            &[
                load(PF_R | PF_X, 0x1000, 0x1_0000, 0x10, 0x10),
                load(PF_R, 0x2800, 0x1_0800, 0x10, 0x10),
            ],
            0x1_0000,
        ),
        ElfError::Overlap,
    );
    expect(
        img(
            &[
                text(),
                load(PF_R | PF_W, 0x2000, 0x1_0000 + 0x1000, 0x10, 0x2000),
                load(PF_R, 0x3000, 0x1_2000, 0x10, 0x10),
            ],
            0x1_0000,
        ),
        ElfError::Overlap,
    );
    expect(img(&[text()], 0x2_0000), ElfError::EntryNotExecutable);
    expect(img(&[text()], 0x1_0100), ElfError::EntryNotExecutable); // one past the segment
    expect(
        img(&[load(PF_R | PF_W, 0x1000, 0x1_0000, 0x10, 0x10)], 0x1_0000),
        ElfError::EntryNotExecutable,
    );
    expect(
        img(
            &[
                load(PF_R | PF_X, 0x1000, 0x1_0000, 0x10, 0x100_0000),
                load(PF_R, 0x1000, 0x400_0000, 0x10, 0x100_0000),
                load(PF_R, 0x2000, 0x800_0000, 0x10, 0x3000_0000),
            ],
            0x1_0000,
        ),
        ElfError::TooBig,
    );
    // program header kinds
    let with = |kind: u32, flags: u32| {
        let mut b = img(
            &[
                text(),
                Ph {
                    kind,
                    flags,
                    offset: 0,
                    vaddr: 0,
                    filesz: 0,
                    memsz: 0,
                },
            ],
            0x1_0000,
        );
        b[56] = 2;
        b
    };
    expect(with(3, 0), ElfError::Dynamic); // PT_INTERP
    expect(with(2, 0), ElfError::Dynamic); // PT_DYNAMIC
    expect(with(0x6474_e551, PF_R | PF_W | PF_X), ElfError::ExecStack);
    assert!(
        parse(&with(0x6474_e551, PF_R | PF_W)).is_ok(),
        "a non-executable GNU_STACK is fine"
    );
    assert!(
        parse(&with(0x7000_0003, 0)).is_ok(),
        "unknown program headers (RISC-V attributes...) are ignored"
    );
}

#[test]
fn an_unaligned_start_within_a_page_is_allowed_when_offset_agrees() {
    // vaddr 0x10010 and offset 0x1010: same position within a page
    let b = {
        let mut b = build(
            0x1_0010,
            &[load(PF_R | PF_X, 0x1010, 0x1_0010, 0x20, 0x20)],
            0x2000,
        );
        b.resize(0x2000, 0);
        b
    };
    let img = parse(&b).unwrap();
    assert_eq!(img.segments[0].pages(), (0x10, 0x11));
}

#[test]
fn mutated_images_never_panic_and_accepted_ones_are_safe_to_map() {
    let base = good();
    for seed in 0..SEEDS * 10 {
        let mut rng = Rng::new(seed);
        let mut b = base.clone();
        for _ in 0..1 + rng.below(6) {
            match rng.below(4) {
                0 => {
                    let i = rng.below(b.len().min(200) as u64) as usize; // mostly in the headers
                    b[i] = rng.next() as u8;
                }
                1 => {
                    let i = rng.below(b.len() as u64) as usize;
                    b[i] ^= 1 << rng.below(8);
                }
                2 => b.truncate(rng.below(b.len() as u64 + 1) as usize),
                _ => {
                    let i = rng.below(b.len().saturating_sub(8).max(1) as u64) as usize;
                    if i + 8 <= b.len() {
                        b[i..i + 8].copy_from_slice(&rng.next().to_le_bytes());
                    }
                }
            }
        }
        if let Ok(img) = parse(&b) {
            for (i, s) in img.segments.iter().enumerate() {
                assert!(
                    s.vaddr >= BASE && s.vaddr + s.memsz <= TOP,
                    "seed {seed}: outside the user half"
                );
                assert!(
                    s.filesz <= s.memsz && s.offset + s.filesz <= b.len() as u64,
                    "seed {seed}: outside the file"
                );
                assert!(!(s.perms.write && s.perms.exec), "seed {seed}: W+X");
                assert!(s.perms.user && s.perms.read, "seed {seed}");
                for t in &img.segments[..i] {
                    let ((a0, a1), (b0, b1)) = (s.pages(), t.pages());
                    assert!(a1 <= b0 || b1 <= a0, "seed {seed}: overlapping pages");
                }
            }
            assert!(
                img.segments
                    .iter()
                    .any(|s| s.perms.exec && (s.vaddr..s.vaddr + s.memsz).contains(&img.entry)),
                "seed {seed}: entry"
            );
        }
    }
}

#[test]
fn a_segment_may_not_touch_the_stack_or_its_guard_page() {
    use abi::user::{STACK_PAGES, STACK_TOP};
    let stack_lo = STACK_TOP - STACK_PAGES * 4096; // lowest stack page
    let guard = stack_lo - 4096;
    let seg = |vaddr: u64, memsz: u64| {
        let mut b = build(
            vaddr,
            &[load(PF_R | PF_X, 0x1000, vaddr, 0x10, memsz)],
            0x2000,
        );
        b.resize(0x2000, 0);
        b
    };
    assert!(
        parse(&seg(guard - 4096, 4096)).is_ok(),
        "ends right below the guard page"
    );
    expect(seg(guard - 4096, 4097), ElfError::OverlapsStack);
    expect(seg(guard, 4096), ElfError::OverlapsStack);
    expect(seg(stack_lo, 4096), ElfError::OverlapsStack);
    expect(seg(STACK_TOP - 4096, 4096), ElfError::OverlapsStack);
    assert!(
        parse(&seg(STACK_TOP, 4096)).is_ok(),
        "above the stack top is fine"
    );
}
