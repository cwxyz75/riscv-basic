// The linker script is found by absolute path (cargo runs rustc from the workspace
// root), like the kernels' build scripts.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/link.ld");
    println!("cargo:rustc-link-arg=--no-relax");
    // The programs are embedded in the boot image: leave out the debug info of the standard
    // library that would otherwise make each one over a megabyte.
    println!("cargo:rustc-link-arg=--strip-debug");
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rerun-if-changed=build.rs");
}
