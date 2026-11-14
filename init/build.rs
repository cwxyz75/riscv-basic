// The linker script is found by absolute path (cargo runs rustc from the workspace
// root), like the kernels' build scripts.
//
// This script also compiles the C test program (S7c) with a RISC-V-capable clang and links it into the
// `ctest` binary through the same cargo/rust-lld link as every other program (OD69). The compiler
// is found from `CC_RISCV` or the usual places; without one the `ctest` program is a stub that
// exits 99, so the suite reports the missing toolchain instead of the build failing.
use std::path::{Path, PathBuf};
use std::process::Command;

fn works(clang: &str) -> bool {
    Command::new(clang)
        .arg("--print-targets")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("riscv64"))
        .unwrap_or(false)
}

fn find_clang() -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(c) = std::env::var("CC_RISCV") {
        if !works(&c) {
            panic!("CC_RISCV={c} is not a clang with a riscv64 target");
        }
        candidates.push(c);
    }
    candidates.extend(
        [
            "/opt/homebrew/opt/llvm/bin/clang",
            "/usr/local/opt/llvm/bin/clang",
            "clang",
        ]
        .map(String::from),
    );
    candidates.into_iter().find(|c| works(c))
}

/// Archivers to try: `llvm-ar` next to `clang`, `llvm-ar` on the path, then plain `ar` (an
/// archive of RISC-V objects is just a file format, any `ar` makes it).
fn archivers(clang: &str) -> Vec<String> {
    let beside = Path::new(clang).with_file_name("llvm-ar");
    let mut v = Vec::new();
    if beside.exists() {
        v.push(beside.to_string_lossy().into_owned());
    }
    v.push("llvm-ar".into());
    v.push("ar".into());
    v
}

/// Compile `sources` with the cross clang (flags shared by every C program here) into objects under
/// `OUT_DIR/<name>/` and archive them as `lib<name>.a`.
fn build_c_archive(name: &str, sources: &[PathBuf], extra: &[String]) -> Option<PathBuf> {
    let clang = find_clang()?;
    let dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = PathBuf::from(std::env::var("OUT_DIR").ok()?).join(name);
    std::fs::create_dir_all(&out).ok()?;
    let mut objs = Vec::new();
    for src in sources {
        let obj = out.join(format!(
            "{}.o",
            src.file_stem()
                .expect("a source file name")
                .to_string_lossy()
        ));
        let ok = Command::new(&clang)
            .args([
                "--target=riscv64-unknown-elf",
                "-march=rv64imac",
                "-mabi=lp64",
                "-ffreestanding",
                "-nostdlibinc",
                "-fno-pic",
                "-fno-stack-protector",
                "-O2",
            ])
            .args(extra)
            .arg(format!("-I{dir}/../libc/include"))
            .arg("-c")
            .arg(src)
            .arg("-o")
            .arg(&obj)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            panic!("{} did not compile", src.display());
        }
        println!("cargo:rerun-if-changed={}", src.display());
        objs.push(obj);
    }
    let archive = out.join(format!("lib{name}.a"));
    let _ = std::fs::remove_file(&archive);
    let made = archivers(&clang).iter().any(|ar| {
        Command::new(ar)
            .arg("rcs")
            .arg(&archive)
            .args(&objs)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    if !made {
        panic!("no archiver (llvm-ar or ar) could make the C archive");
    }
    Some(archive)
}

fn build_c_tests(dir: &str) -> Option<PathBuf> {
    build_c_archive(
        "ctests",
        &[Path::new(dir).join("../libc/tests/c/transcript.c")],
        &["-Wall".to_string()],
    )
}

/// Lua 5.4 (S7d): every upstream source but the compiler front end `luac.c`, unmodified, built with the
/// settings header the host oracle also uses.
fn build_lua(dir: &str) -> Option<PathBuf> {
    let src = Path::new(dir).join("../third_party/lua/src");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&src)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|e| e == "c") && p.file_name().is_some_and(|n| n != "luac.c")
        })
        .collect();
    files.sort();
    let conf = Path::new(dir).join("../third_party/lua/os_lua.h");
    println!("cargo:rerun-if-changed={}", conf.display());
    build_c_archive(
        "lua",
        &files,
        &[
            "-w".to_string(),
            "-O3".to_string(),
            format!("-include{}", conf.display()),
        ],
    )
}

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/link.ld");
    println!("cargo:rustc-link-arg=--no-relax");
    // The programs are embedded in the boot image: leave out the debug info of the standard
    // library that would otherwise make each one over a megabyte.
    println!("cargo:rustc-link-arg=--strip-debug");
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC_RISCV");
    println!("cargo:rerun-if-changed=../libc/include");
    println!("cargo:rustc-check-cfg=cfg(have_c)");
    println!("cargo:rustc-check-cfg=cfg(have_lua)");
    if let Some(archive) = build_c_tests(&dir) {
        println!("cargo:rustc-cfg=have_c");
        println!("cargo:rustc-link-arg-bin=ctest={}", archive.display());
        if let Some(lua) = build_lua(&dir) {
            println!("cargo:rustc-cfg=have_lua");
            println!("cargo:rustc-link-arg-bin=lua={}", lua.display());
        }
    } else {
        println!(
            "cargo:warning=no RISC-V C compiler found (set CC_RISCV): `ctest` and `lua` are stubs"
        );
    }
}
