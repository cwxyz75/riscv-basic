// Cargo runs rustc from the workspace root, so a relative `-Tlinker.ld` would not be
// found; point the linker at the script by absolute path instead.
//
// The user programs (init/, built by `make os-user` for the user target) are embedded in
// the kernel image. If they have not been built yet (a bare `cargo clippy` here, say) an
// empty file stands in, and the kernel reports that no user program is available.
use std::path::PathBuf;

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    // We never initialise the global pointer register, so stop the linker from
    // relaxing accesses into gp-relative form.
    println!("cargo:rustc-link-arg=--no-relax");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=build.rs");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let built = PathBuf::from(&dir).join("../target/riscv64imac-unknown-none-elf/debug");
    for name in ["init", "crash"] {
        let src = built.join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        let bytes = std::fs::read(&src).unwrap_or_default();
        std::fs::write(out.join(format!("{name}.elf")), bytes).expect("write the embedded program");
    }
}
