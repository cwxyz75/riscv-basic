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
    b[16] = 4; // ET_CORE (ET_DYN is a static-PIE now: HD2)
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
                    img.pie || (s.vaddr >= BASE && s.vaddr + s.memsz <= TOP),
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

// ---- HD2: static-PIE (ET_DYN) images ----

const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;

/// A static-PIE: text at 0x0 (file 0x1000), data at 0x2000 (file 0x2000) holding a dynamic section at 0x2000 and a relocation table at 0x2100.
/// `dynamic` is the dynamic section's entries, `relas` the relocations as (offset, info, addend).
fn pie(dynamic: &[(u64, u64)], relas: &[(u64, u64, u64)]) -> Vec<u8> {
    let phs = [
        load(PF_R | PF_X, 0x1000, 0x0, 0x800, 0x800),
        load(PF_R | PF_W, 0x2000, 0x2000, 0x400, 0x3000),
        Ph {
            kind: 2, // PT_DYNAMIC
            flags: PF_R | PF_W,
            offset: 0x2000,
            vaddr: 0x2000,
            filesz: (dynamic.len() * 16) as u64,
            memsz: (dynamic.len() * 16) as u64,
        },
    ];
    let mut b = build(0x10, &phs, 0);
    b[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
    b.resize(0x2400, 0);
    for (i, (tag, val)) in dynamic.iter().enumerate() {
        b[0x2000 + i * 16..0x2000 + i * 16 + 8].copy_from_slice(&tag.to_le_bytes());
        b[0x2000 + i * 16 + 8..0x2000 + i * 16 + 16].copy_from_slice(&val.to_le_bytes());
    }
    for (i, (off, info, add)) in relas.iter().enumerate() {
        let at = 0x2100 + i * 24;
        b[at..at + 8].copy_from_slice(&off.to_le_bytes());
        b[at + 8..at + 16].copy_from_slice(&info.to_le_bytes());
        b[at + 16..at + 24].copy_from_slice(&add.to_le_bytes());
    }
    b
}

fn good_dyn() -> Vec<(u64, u64)> {
    vec![
        (DT_RELA, 0x2100),
        (DT_RELASZ, 48),
        (DT_RELAENT, 24),
        (DT_NULL, 0),
    ]
}

fn good_pie() -> Vec<u8> {
    pie(&good_dyn(), &[(0x2200, 3, 0x40), (0x2208, 3, 0x1000)])
}

#[test]
fn a_static_pie_with_relative_relocations_is_accepted_and_reports_its_span() {
    let img = parse(&good_pie()).unwrap();
    assert!(img.pie);
    assert_eq!(img.entry, 0x10);
    assert_eq!(img.span, 0x5000, "the data segment ends at 0x2000 + 0x3000");
    assert_eq!(img.segments.len(), 2);
    assert_eq!(
        img.segments[0].vaddr, 0,
        "link addresses are offsets from the base"
    );
    // an ET_EXEC image is not one
    let e = parse(&good()).unwrap();
    assert!((!e.pie, e.span) == (true, 0));
}

/// The loader applies the relocations to its copy of the bytes: in the writable data and in the read-only text alike (a prebuilt library's absolute words).
#[test]
fn the_loader_applies_relocations_to_its_copy_of_each_segment() {
    let b = pie(&good_dyn(), &[(0x2200, 3, 0x40), (0x100, 3, 0x1000)]);
    let img = parse(&b).unwrap();
    let base = 0x40_0000_0000u64;
    for seg in &img.segments {
        let mut buf = b[seg.offset as usize..(seg.offset + seg.filesz) as usize].to_vec();
        img.relocate(seg, &mut buf, base);
        let word = |off: u64| {
            u64::from_le_bytes(buf[(off - seg.vaddr) as usize..][..8].try_into().unwrap())
        };
        if seg.vaddr == 0 {
            assert_eq!(word(0x100), base + 0x1000);
        } else {
            assert_eq!(word(0x2200), base + 0x40);
            // a word that is no relocation stays as the file had it
            assert_eq!(word(0x2208), 0);
        }
        // a buffer that is not the segment's bytes is left alone
        let mut short = vec![0u8; 3];
        img.relocate(seg, &mut short, base);
        assert_eq!(short, vec![0u8; 3]);
    }
}

#[test]
fn a_static_pie_with_no_relocations_is_accepted() {
    let b = pie(&[(DT_NULL, 0)], &[]);
    assert!(parse(&b).unwrap().pie);
}

#[test]
fn every_static_pie_rule_has_a_rejection() {
    let bad = |dynamic: &[(u64, u64)], relas: &[(u64, u64, u64)], want: ElfError| {
        assert_eq!(parse(&pie(dynamic, relas)), Err(want));
    };
    // a needed library, or any tag outside the allowed few
    bad(&[(DT_NEEDED, 1), (DT_NULL, 0)], &[], ElfError::Dynamic);
    bad(&[(17, 0x2100), (DT_NULL, 0)], &[], ElfError::Dynamic); // DT_REL
    bad(&[(23, 0x2100), (DT_NULL, 0)], &[], ElfError::Dynamic); // DT_JMPREL
                                                                // no terminating DT_NULL
    bad(
        &[(DT_RELA, 0x2100), (DT_RELASZ, 0), (DT_RELAENT, 24)],
        &[],
        ElfError::Dynamic,
    );
    // a wrong entry size, a table size that is not a whole number of entries
    bad(
        &[
            (DT_RELA, 0x2100),
            (DT_RELASZ, 24),
            (DT_RELAENT, 12),
            (DT_NULL, 0),
        ],
        &[(0x2200, 3, 0)],
        ElfError::Dynamic,
    );
    bad(
        &[
            (DT_RELA, 0x2100),
            (DT_RELASZ, 30),
            (DT_RELAENT, 24),
            (DT_NULL, 0),
        ],
        &[(0x2200, 3, 0)],
        ElfError::Dynamic,
    );
    // a relocation that is not R_RISCV_RELATIVE: R_RISCV_64 (2), a symbolic one (symbol 1 in the high word), type 0
    for info in [2u64, 3 | (1 << 32), 0] {
        bad(
            &[
                (DT_RELA, 0x2100),
                (DT_RELASZ, 24),
                (DT_RELAENT, 24),
                (DT_NULL, 0),
            ],
            &[(0x2200, info, 0)],
            ElfError::Relocation,
        );
    }
    // a word outside the file bytes of every segment (in the zero-filled tail, past the image, unaligned, straddling the end of the file bytes)
    for off in [0x2400u64, 0x5000, 0x2201, 0x23fc, 0x4ffc, u64::MAX - 3] {
        bad(
            &[
                (DT_RELA, 0x2100),
                (DT_RELASZ, 24),
                (DT_RELAENT, 24),
                (DT_NULL, 0),
            ],
            &[(off, 3, 0)],
            ElfError::Relocation,
        );
    }
    // an addend that points beyond the image
    bad(
        &[
            (DT_RELA, 0x2100),
            (DT_RELASZ, 24),
            (DT_RELAENT, 24),
            (DT_NULL, 0),
        ],
        &[(0x2200, 3, 0x5001)],
        ElfError::Relocation,
    );
    // a table that is not in the file bytes of a loaded segment
    bad(
        &[
            (DT_RELA, 0x4000),
            (DT_RELASZ, 24),
            (DT_RELAENT, 24),
            (DT_NULL, 0),
        ],
        &[(0x2200, 3, 0)],
        ElfError::Relocation,
    );
    bad(
        &[
            (DT_RELA, u64::MAX - 8),
            (DT_RELASZ, 24),
            (DT_RELAENT, 24),
            (DT_NULL, 0),
        ],
        &[(0x2200, 3, 0)],
        ElfError::Relocation,
    );
}

#[test]
fn a_dynamic_section_is_refused_in_a_fixed_image_and_an_interpreter_everywhere() {
    // ET_EXEC with a PT_DYNAMIC: still the old refusal
    let phs = [
        load(PF_R | PF_X, 0x1000, 0x1_0000, 0x800, 0x800),
        Ph {
            kind: 2,
            flags: PF_R,
            offset: 0x2000,
            vaddr: 0x1_2000,
            filesz: 16,
            memsz: 16,
        },
    ];
    let b = build(0x1_0010, &phs, 0x2200);
    assert_eq!(parse(&b), Err(ElfError::Dynamic));
    // a static-PIE with a PT_INTERP
    let phs = [
        load(PF_R | PF_X, 0x1000, 0x0, 0x800, 0x800),
        Ph {
            kind: 3,
            flags: PF_R,
            offset: 0,
            vaddr: 0,
            filesz: 0,
            memsz: 0,
        },
    ];
    let mut b = build(0x10, &phs, 0x2200);
    b[16..18].copy_from_slice(&3u16.to_le_bytes());
    assert_eq!(parse(&b), Err(ElfError::Dynamic));
    // two dynamic headers
    let mut b = pie(&good_dyn(), &[]);
    b[56..58].copy_from_slice(&4u16.to_le_bytes());
    let extra = {
        let mut h = vec![0u8; 56];
        h[0..4].copy_from_slice(&2u32.to_le_bytes());
        h
    };
    let at = 64 + 3 * 56;
    b.splice(at..at, extra);
    assert_eq!(parse(&b), Err(ElfError::Dynamic));
}

#[test]
fn a_static_pie_too_big_for_the_user_half_is_refused() {
    let phs = [load(PF_R | PF_X, 0x1000, 0x0, 0x800, MAX_PIE_SPAN + 4096)];
    let mut b = build(0x10, &phs, 0x2200);
    b[16..18].copy_from_slice(&3u16.to_le_bytes());
    b.resize(0x2100, 0);
    assert_eq!(parse(&b), Err(ElfError::SegmentOutsideUser));
}

#[test]
fn mutated_static_pies_never_panic_and_accepted_ones_are_safe_to_relocate() {
    let base = good_pie();
    for seed in 0..SEEDS * 10 {
        let mut rng = Rng::new(seed ^ 0x9e37_79b9);
        let mut b = base.clone();
        for _ in 0..1 + rng.below(6) {
            if b.is_empty() {
                break;
            }
            match rng.below(4) {
                0 => {
                    let i = rng.below(b.len().min(0x2200) as u64) as usize;
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
            if img.pie {
                assert!(img.span <= MAX_PIE_SPAN + 4096, "seed {seed}: span");
                for s in &img.segments {
                    assert!(
                        s.vaddr + s.memsz <= img.span,
                        "seed {seed}: a segment outside the span"
                    );
                    assert!(!(s.perms.write && s.perms.exec), "seed {seed}: W+X");
                }
            }
        }
    }
}
