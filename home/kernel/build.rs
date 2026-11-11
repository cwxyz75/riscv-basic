// Cargo runs rustc from the workspace root, so a relative `-Tlinker.ld` would
// not be found; point the linker at the script by absolute path instead.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    println!("cargo:rustc-link-arg=-T{dir}/linker.ld");
    // We never initialise the global pointer register, so stop the linker from
    // relaxing accesses into gp-relative form.
    println!("cargo:rustc-link-arg=--no-relax");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=build.rs");
}
