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
    let mut image_files: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for name in ["suite", "init", "crash", "hello", "launcher", "memfsd"] {
        let src = built.join(name);
        println!("cargo:rerun-if-changed={}", src.display());
        let bytes = std::fs::read(&src).unwrap_or_default();
        if name != "suite" {
            image_files.push((format!("bin/{name}").into_bytes(), bytes.clone()));
        }
        std::fs::write(out.join(format!("{name}.elf")), bytes).expect("write the embedded program");
    }
    // The boot image: every program (not the test suite) plus the fixture files. A missing
    // program is an empty entry, so a bare `cargo clippy` still builds.
    let fixtures = PathBuf::from(&dir).join("../bootfs");
    println!("cargo:rerun-if-changed={}", fixtures.display());
    // Programs are `bin/<name>`; everything under `bootfs/` keeps its relative path (`etc/trusted`,
    // `home/hello.txt`): an image name is the absolute path of the file without the leading slash.
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<(Vec<u8>, Vec<u8>)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = format!("{prefix}/{}", e.file_name().to_string_lossy());
            if e.path().is_dir() {
                walk(&e.path(), &name, out);
            } else if e.path().is_file() {
                out.push((
                    name.into_bytes(),
                    std::fs::read(e.path()).unwrap_or_default(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    walk(&fixtures, "", &mut files);
    for (n, d) in files {
        // walk() prefixes every name with "/": drop it
        image_files.push((n[1..].to_vec(), d));
    }
    let refs: Vec<(&[u8], &[u8])> = image_files
        .iter()
        .map(|(n, d)| (n.as_slice(), d.as_slice()))
        .collect();
    let image = proto::bootimg::build(&refs).expect("build the boot image");
    std::fs::write(out.join("bootimg.bin"), image).expect("write the boot image");
}
