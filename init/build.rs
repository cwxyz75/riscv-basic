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
        // (named by the whole path: BusyBox has a `time.c` in two directories)
        let stem = src
            .with_extension("")
            .to_string_lossy()
            .replace(['/', '\\', ':'], "_");
        let obj = out.join(format!("{stem}.o"));
        let ok = Command::new(&clang)
            .args([
                "--target=riscv64-unknown-elf",
                "-march=rv64gc",
                "-mabi=lp64d",
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
        &[
            Path::new(dir).join("../libc/tests/c/transcript.c"),
            Path::new(dir).join("../libc/tests/c/sockets.c"),
            Path::new(dir).join("../libc/tests/c/mount.c"),
            Path::new(dir).join("../libc/tests/c/signals.c"),
        ],
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

/// kilo (a 1300-line editor): the single file fetched by `make fetch-third-party`; without it `kilo` is a stub.
fn build_kilo(dir: &str) -> Option<PathBuf> {
    let src = Path::new(dir).join("../third_party/fetched/kilo/src/kilo.c");
    println!("cargo:rerun-if-changed={}", src.display());
    if !src.exists() {
        return None;
    }
    build_c_archive("kilo", &[src], &["-w".to_string(), "-O2".to_string()])
}

/// BusyBox 1.36.1 (UV8): the sources fetched by `make fetch-third-party` (not committed), built for the configuration in
/// `third_party/busybox/config` from the file list and generated headers committed next to it (`scripts/busybox_configure.sh`
/// makes them). Without the fetched sources, or without a RISC-V C compiler, `busybox` is a stub that exits 99.
fn build_busybox(dir: &str) -> Option<PathBuf> {
    let root = Path::new(dir).join("../third_party");
    let src = root.join("fetched/busybox/src");
    // (so that fetching the sources later turns the stub into the program)
    println!(
        "cargo:rerun-if-changed={}",
        src.join("libbb/appletlib.c").display()
    );
    if !src.join("libbb/appletlib.c").exists() {
        return None;
    }
    let list = std::fs::read_to_string(root.join("busybox/files.txt")).ok()?;
    let files: Vec<PathBuf> = list
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| src.join(l))
        .collect();
    println!(
        "cargo:rerun-if-changed={}",
        root.join("busybox/files.txt").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        root.join("busybox/gen").display()
    );
    // objects of different directories can share a file name (`md5_sha1_sum`, `ls`): build_c_archive names objects by stem
    let _ = &files;
    build_c_archive(
        "busybox",
        &files,
        &[
            "-w".to_string(),
            "-Os".to_string(),
            "-std=gnu99".to_string(),
            "-funsigned-char".to_string(),
            "-ffunction-sections".to_string(),
            "-fdata-sections".to_string(),
            "-fno-builtin-strlen".to_string(),
            "-D_GNU_SOURCE".to_string(),
            "-DNDEBUG".to_string(),
            // clang puts an uninitialised `const` object in .rodata (gcc in .bss); BusyBox writes `ptr_to_globals` once at start
            "-DBB_GLOBAL_CONST=".to_string(),
            "-DBB_VER=\"1.36.1\"".to_string(),
            "-DBB_BT=\"os\"".to_string(),
            "-DKBUILD_STR(s)=#s".to_string(),
            "-DKBUILD_BASENAME=KBUILD_STR(bb)".to_string(),
            "-DKBUILD_MODNAME=KBUILD_STR(bb)".to_string(),
            format!("-I{}", root.join("busybox/gen").display()),
            format!("-I{}", src.join("include").display()),
            format!("-I{}", src.join("libbb").display()),
            format!("-include{}", root.join("busybox/gen/autoconf.h").display()),
        ],
    )
}

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/link.ld");
    println!("cargo:rustc-link-arg=--no-relax");
    // The programs are embedded in the boot image: leave out the debug info of the standard
    // library that would otherwise make each one over a megabyte.
    // (no symbol tables either: the image is held in several copies, so each program's 1 MiB of symbols cost several; on CI's machine
    // the tables were twice the size and the hog test found 26 MiB of heap left instead of 120)
    println!("cargo:rustc-link-arg=--strip-all");
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CC_RISCV");
    println!("cargo:rerun-if-changed=../libc/include");
    println!("cargo:rustc-check-cfg=cfg(have_c)");
    println!("cargo:rustc-check-cfg=cfg(have_lua)");
    println!("cargo:rustc-check-cfg=cfg(have_busybox)");
    println!("cargo:rustc-check-cfg=cfg(have_kilo)");
    if let Some(archive) = build_c_tests(&dir) {
        println!("cargo:rustc-cfg=have_c");
        println!("cargo:rustc-link-arg-bin=ctest={}", archive.display());
        if let Some(lua) = build_lua(&dir) {
            println!("cargo:rustc-cfg=have_lua");
            println!("cargo:rustc-link-arg-bin=lua={}", lua.display());
        }
        if let Some(k) = build_kilo(&dir) {
            println!("cargo:rustc-cfg=have_kilo");
            println!("cargo:rustc-link-arg-bin=kilo={}", k.display());
        }
        if let Some(bb) = build_busybox(&dir) {
            println!("cargo:rustc-cfg=have_busybox");
            println!("cargo:rustc-link-arg-bin=busybox={}", bb.display());
            println!("cargo:rustc-link-arg-bin=busybox=--error-limit=0");
        }
    } else {
        println!(
            "cargo:warning=no RISC-V C compiler found (set CC_RISCV): `ctest` and `lua` are stubs"
        );
    }
}
