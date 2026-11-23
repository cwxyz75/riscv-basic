//! Building the library: generate its source from `abi::syscall::TABLE`, run `rustc` for the user target, check the result.

use crate::{image, page, symbol_name, Image};
use std::path::Path;
use std::process::Command;
use std::string::String;
use std::vec::Vec;

/// The user target the library is built for.
pub const TARGET: &str = "riscv64gc-unknown-none-elf";

/// The linker script: one read-and-execute segment holding everything (headers, `.dynsym`, `.hash`, `.dynstr`, the code, `.dynamic`) and one read-only segment of
/// one page of no file bytes, the data page.
const LINKER_SCRIPT: &str = r#"OUTPUT_ARCH(riscv)
PHDRS {
  image PT_LOAD FILEHDR PHDRS FLAGS(5);
  data PT_LOAD FLAGS(4);
  dynamic PT_DYNAMIC FLAGS(4);
}
SECTIONS {
  . = SIZEOF_HEADERS;
  .dynsym : { *(.dynsym) } :image
  .hash : { *(.hash) } :image
  .dynstr : { *(.dynstr) } :image
  .text : { *(.text .text.*) } :image
  .dynamic : { *(.dynamic) } :image :dynamic
  .libsys_data (NOLOAD) : ALIGN(4K) { *(.libsys_data) } :data
  /DISCARD/ : { *(.gnu.hash .comment .eh_frame .riscv.attributes .relro_padding .got .got.plt .data .data.* .bss .bss.*) }
}
"#;

/// The Rust source of the library (a `no_std`, `no_main` crate that is all assembly).
pub fn source() -> String {
    let mut s = String::from(
        "#![no_std]\n#![no_main]\nuse core::arch::global_asm;\n#[panic_handler]\nfn panic(_: &core::panic::PanicInfo) -> ! {\n    loop {}\n}\nglobal_asm!(r#\"\n",
    );
    s.push_str("    .section .libsys_data,\"a\",@nobits\n    .balign 4096\n    .globl libsys_data\n    .type libsys_data,@object\nlibsys_data:\n.Ldata:\n    .space 4096\n    .size libsys_data, 4096\n    .section .text\n");
    for &(name, number) in abi::syscall::TABLE {
        let sym = symbol_name(name);
        if number == abi::syscall::CLOCK_GET {
            // computed here, in user mode, from the `time` counter and the data page: no trap (`clock_get` is `((time - base) * mult) >> shift`, the product taken in 128 bits)
            s.push_str(&std::format!(
                "    .globl {sym}\n    .type {sym},@function\n{sym}:\n    lla t0, .Ldata\n    rdtime t1\n    ld t2, {base}(t0)\n    ld t3, {mult}(t0)\n    ld t6, {shift}(t0)\n    sub t1, t1, t2\n    mul t4, t1, t3\n    mulhu t5, t1, t3\n    srl t4, t4, t6\n    li t2, 64\n    sub t2, t2, t6\n    sll t5, t5, t2\n    or a1, t4, t5\n    li a0, 0\n    ret\n    .size {sym}, .-{sym}\n",
                base = page::TIME_BASE * 8,
                mult = page::TIME_MULT * 8,
                shift = page::TIME_SHIFT * 8,
            ));
            continue;
        }
        s.push_str(&std::format!(
            "    .globl {sym}\n    .type {sym},@function\n{sym}:\n    li a7, {number}\n    ecall\n    ret\n    .size {sym}, .-{sym}\n"
        ));
    }
    // the raw entry: the number in a6, for a call this table does not name (the boot-time suite's bad-number tests)
    s.push_str("    .globl sys_raw\n    .type sys_raw,@function\nsys_raw:\n    mv a7, a6\n    ecall\n    ret\n    .size sys_raw, .-sys_raw\n");
    s.push_str("\"#);\n");
    s
}

/// Build the library in `dir` with `rustc` (the `RUSTC` environment variable, else `rustc`) and return the object's bytes, checked.
pub fn build(dir: &Path) -> Result<Vec<u8>, String> {
    std::fs::create_dir_all(dir).map_err(|e| std::format!("create {}: {e}", dir.display()))?;
    let (src, ld, out) = (
        dir.join("libsys.rs"),
        dir.join("libsys.ld"),
        dir.join("libsys.so"),
    );
    std::fs::write(&src, source()).map_err(|e| e.to_string())?;
    std::fs::write(&ld, LINKER_SCRIPT).map_err(|e| e.to_string())?;
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let status = Command::new(rustc)
        .args([
            "--target",
            TARGET,
            "--crate-type",
            "bin",
            "--crate-name",
            "libsys",
            "--edition",
            "2021",
        ])
        .args([
            "-C",
            "panic=abort",
            "-C",
            "opt-level=2",
            "-C",
            "relocation-model=pic",
            "-C",
            "debuginfo=0",
        ])
        .args([
            "-C",
            "link-arg=-shared",
            "-C",
            "link-arg=-znorelro",
            "-C",
            "link-arg=--no-relax",
            "-C",
            "link-arg=--strip-all",
        ])
        .arg(std::format!("-Clink-arg=-T{}", ld.display()))
        .arg(&src)
        .arg("-o")
        .arg(&out)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .status()
        .map_err(|e| std::format!("run rustc: {e}"))?;
    if !status.success() {
        return Err(std::format!("rustc failed for the library ({status})"));
    }
    let bytes = std::fs::read(&out).map_err(|e| e.to_string())?;
    check(&bytes)?;
    Ok(bytes)
}

/// The checks of AB7's acceptance on a built object: it is a shared object whose first segment holds the symbols, it needs no relocation (no relocation
/// section, no relocation tag in `.dynamic`), every call in the table has a symbol (and `sys_raw` is there), and the data page is a symbol of a
/// segment of one page with no file bytes, readable and not writable.
pub fn check(bytes: &[u8]) -> Result<(), String> {
    let len = image::image_len(bytes).ok_or("not the library's first segment")?;
    let img = Image::new(bytes.get(..len).ok_or("the first segment is cut off")?)
        .map_err(|e| std::format!("{e:?}"))?;
    for &(name, _) in abi::syscall::TABLE {
        let sym = symbol_name(name);
        let s = img
            .lookup(&sym)
            .ok_or_else(|| std::format!("no symbol {sym}"))?;
        if s.value as usize >= len || s.value % 2 != 0 {
            return Err(std::format!(
                "{sym} is outside the code segment ({:#x})",
                s.value
            ));
        }
    }
    img.lookup("sys_raw").ok_or("no symbol sys_raw")?;
    let data = img.lookup("libsys_data").ok_or("no symbol libsys_data")?;
    if data.value % page::SIZE as u64 != 0
        || data.size != page::SIZE as u64
        || (data.value as usize) < len
    {
        return Err(std::format!(
            "the data page is misplaced ({:#x}, {} bytes)",
            data.value,
            data.size
        ));
    }
    // section names: no relocation sections (a stripped object has few sections; look at their type)
    let shoff = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    let (shentsize, shnum) = (
        u16::from_le_bytes(bytes[58..60].try_into().unwrap()) as usize,
        u16::from_le_bytes(bytes[60..62].try_into().unwrap()) as usize,
    );
    for i in 0..shnum {
        let at = shoff + i * shentsize;
        let ty = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
        if ty == 4 || ty == 9 {
            return Err(std::format!(
                "section {i} is a relocation section (type {ty})"
            ));
        }
    }
    // the segments: the data page is read-only and has no file bytes
    let phnum = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;
    let mut loads = 0;
    for i in 0..phnum {
        let at = 64 + i * 56;
        let (ty, flags) = (
            u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()),
            u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()),
        );
        let (vaddr, filesz, memsz) = (
            u64::from_le_bytes(bytes[at + 16..at + 24].try_into().unwrap()),
            u64::from_le_bytes(bytes[at + 32..at + 40].try_into().unwrap()),
            u64::from_le_bytes(bytes[at + 40..at + 48].try_into().unwrap()),
        );
        if ty == 1 {
            loads += 1;
            if flags & 2 != 0 {
                return Err("a segment is writable".into());
            }
            if vaddr == data.value && (flags != 4 || filesz != 0 || memsz != page::SIZE as u64) {
                return Err(std::format!(
                    "the data segment is wrong (flags {flags}, file {filesz}, mem {memsz})"
                ));
            }
        }
    }
    if loads != 2 {
        return Err(std::format!(
            "expected two loadable segments, found {loads}"
        ));
    }
    Ok(())
}
