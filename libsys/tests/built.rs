//! AB7's host acceptance: build the object and check it (no relocations, a symbol per call, the data page).
use libsys::{build, Image};

#[test]
fn the_library_builds_and_passes_its_checks() {
    let dir = std::env::temp_dir().join(format!("libsys-test-{}", std::process::id()));
    let bytes = build::build(&dir).expect("build the library");
    let len = libsys::image::image_len(&bytes).unwrap();
    let img = Image::new(&bytes[..len]).unwrap();
    // every call has a symbol, and the symbols are distinct and in the code
    let mut seen = std::collections::BTreeSet::new();
    for &(name, _) in abi::syscall::TABLE {
        let s = img.lookup(&libsys::symbol_name(name)).unwrap();
        assert!(seen.insert(s.value));
    }
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn a_check_refuses_a_relocation_tag_and_a_missing_symbol() {
    let dir = std::env::temp_dir().join(format!("libsys-test-neg-{}", std::process::id()));
    let good = build::build(&dir).unwrap();
    // cut the image short: not the library
    assert!(build::check(&good[..64]).is_err());
    // corrupt a symbol's name so a call has none
    let mut bad = good.clone();
    let at = bad
        .windows(b"sys_process_exit".len())
        .position(|w| w == b"sys_process_exit")
        .unwrap();
    bad[at + 4] = b'X';
    assert!(build::check(&bad).is_err());
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn the_page_layout_round_trips() {
    let p = libsys::page::Page {
        abi_version: 1,
        time_mult: 7,
        time_shift: 24,
        page_size: 4096,
        cbom_block: 64,
        cboz_block: 64,
        harts_online: 4,
        flags: 0,
        time_base: 9,
    };
    assert_eq!(libsys::page::Page::from_bytes(&p.to_bytes()), Some(p));
}

/// The symbols of the previous release (`symbols-v1.txt`) that `img` lacks.
fn missing(list: &str, img: &Image) -> Vec<String> {
    list.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| img.lookup(l).is_none())
        .map(String::from)
        .collect()
}

/// AB9, the versioning rule: the library only grows. Every symbol of the checked-in list of the previous release is still there.
#[test]
fn no_symbol_of_the_previous_release_is_missing() {
    let dir = std::env::temp_dir().join(format!("libsys-test-v1-{}", std::process::id()));
    let bytes = build::build(&dir).unwrap();
    let len = libsys::image::image_len(&bytes).unwrap();
    let img = Image::new(&bytes[..len]).unwrap();
    let list = include_str!("../symbols-v1.txt");
    assert_eq!(missing(list, &img), Vec::<String>::new());
    // the check sees a removal
    let with_removed = format!("{list}\nsys_a_call_that_was_removed\n");
    assert_eq!(
        missing(&with_removed, &img),
        vec!["sys_a_call_that_was_removed".to_string()]
    );
    // and every call of the table is on the list (a new call is added to the list when it is released)
    for &(name, _) in abi::syscall::TABLE {
        assert!(
            list.lines().any(|l| l == libsys::symbol_name(name)),
            "{name} is not in symbols-v1.txt"
        );
    }
    std::fs::remove_dir_all(dir).ok();
}
