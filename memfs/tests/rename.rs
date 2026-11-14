//! memfs `rename`: hand-derived scenarios (POSIX `rename(2)` semantics, written from the manual page,
//! not from the implementation).

use memfs::{Error, Fs, Kind, Limits, ROOT};

fn tree() -> (Fs, u32, u32, u32) {
    let mut fs = Fs::new(Limits::default());
    let a = fs.mkdir(ROOT, b"a").unwrap();
    let b = fs.mkdir(ROOT, b"b").unwrap();
    let f = fs.create_file(a, b"f").unwrap();
    fs.write(f, 0, b"data").unwrap();
    (fs, a, b, f)
}

#[test]
fn a_file_moves_within_and_between_directories_keeping_its_node() {
    let (mut fs, a, b, f) = tree();
    fs.rename(a, b"f", a, b"g").unwrap();
    assert_eq!(fs.lookup(a, b"f"), Err(Error::NotFound));
    assert_eq!(fs.lookup(a, b"g"), Ok(f));
    fs.rename(a, b"g", b, b"h").unwrap();
    assert_eq!(fs.lookup(a, b"g"), Err(Error::NotFound));
    assert_eq!(fs.lookup(b, b"h"), Ok(f));
    assert_eq!(fs.read(f, 0, 10).unwrap(), b"data");
}

#[test]
fn renaming_onto_itself_changes_nothing() {
    let (mut fs, a, _b, f) = tree();
    fs.rename(a, b"f", a, b"f").unwrap();
    assert_eq!(fs.lookup(a, b"f"), Ok(f));
}

#[test]
fn a_file_replaces_a_file_and_the_old_one_is_freed() {
    let (mut fs, a, b, f) = tree();
    let old = fs.create_file(b, b"f").unwrap();
    let before = fs.node_count();
    fs.rename(a, b"f", b, b"f").unwrap();
    assert_eq!(fs.lookup(b, b"f"), Ok(f));
    assert_eq!(fs.lookup(a, b"f"), Err(Error::NotFound));
    assert_eq!(fs.node_count(), before - 1);
    assert_eq!(fs.kind(old), Err(Error::NotFound));
}

#[test]
fn a_replaced_file_that_is_held_open_lives_until_released() {
    let (mut fs, a, b, _f) = tree();
    let old = fs.create_file(b, b"f").unwrap();
    fs.retain(old).unwrap();
    fs.rename(a, b"f", b, b"f").unwrap();
    assert_eq!(fs.kind(old), Ok(Kind::File));
    fs.release(old);
    assert_eq!(fs.kind(old), Err(Error::NotFound));
}

#[test]
fn kinds_must_match_when_replacing() {
    let (mut fs, a, b, _f) = tree();
    let d = fs.mkdir(b, b"d").unwrap();
    assert_eq!(fs.rename(a, b"f", b, b"d"), Err(Error::IsDirectory));
    fs.create_file(b, b"plain").unwrap();
    assert_eq!(fs.rename(b, b"d", b, b"plain"), Err(Error::NotDirectory));
    // an empty directory is replaced by a directory; a non-empty one is not
    let e = fs.mkdir(a, b"e").unwrap();
    assert_eq!(fs.rename(a, b"e", b, b"d"), Ok(()));
    assert_eq!(fs.lookup(b, b"d"), Ok(e));
    assert_eq!(fs.kind(d), Err(Error::NotFound));
    fs.create_file(e, b"inside").unwrap();
    fs.mkdir(a, b"e2").unwrap();
    assert_eq!(fs.rename(a, b"e2", b, b"d"), Err(Error::NotEmpty));
}

#[test]
fn a_directory_cannot_move_below_itself() {
    let (mut fs, a, _b, _f) = tree();
    let sub = fs.mkdir(a, b"sub").unwrap();
    assert_eq!(fs.rename(ROOT, b"a", a, b"x"), Err(Error::Invalid));
    assert_eq!(fs.rename(ROOT, b"a", sub, b"x"), Err(Error::Invalid));
    // but it can move sideways, taking its contents along
    let b = fs.lookup(ROOT, b"b").unwrap();
    fs.rename(ROOT, b"a", b, b"moved").unwrap();
    assert_eq!(fs.lookup(b, b"moved"), Ok(a));
    assert_eq!(fs.lookup(a, b"sub"), Ok(sub));
}

#[test]
fn errors() {
    let (mut fs, a, b, _f) = tree();
    assert_eq!(fs.rename(a, b"nope", b, b"x"), Err(Error::NotFound));
    assert_eq!(fs.rename(a, b"f", b, b"bad/name"), Err(Error::BadName));
    assert_eq!(fs.rename(a, b"..", b, b"x"), Err(Error::BadName));
    let file = fs.lookup(a, b"f").unwrap();
    assert_eq!(fs.rename(a, b"f", file, b"x"), Err(Error::NotDirectory));
    // read-only directories refuse both ends
    fs.create_file(b, b"keep").unwrap();
    fs.set_readonly(b).unwrap();
    assert_eq!(fs.rename(a, b"f", b, b"x"), Err(Error::ReadOnly));
    fs.create_file(a, b"g").unwrap();
    assert_eq!(fs.rename(b, b"keep", a, b"x"), Err(Error::ReadOnly));
    // an orphaned directory (unlinked while held) takes nothing
    let orphan = fs.mkdir(a, b"orphan").unwrap();
    fs.retain(orphan).unwrap();
    fs.unlink(a, b"orphan").unwrap();
    assert_eq!(fs.rename(a, b"g", orphan, b"x"), Err(Error::NotFound));
}

#[test]
fn name_bytes_are_charged_to_the_quota() {
    let limits = Limits {
        // two nodes of 96 bytes overhead, the names `d` and `f`, and room for `f` to grow by 9 bytes
        max_meta: 96 * 2 + 2 + 9,
        ..Limits::default()
    };
    let mut fs = Fs::new(limits);
    let d = fs.mkdir(ROOT, b"d").unwrap(); // 1 + 96
    let f = fs.create_file(d, b"f").unwrap(); // 1 + 96
                                              // growing the name by 9 bytes still fits (the budget has exactly 9 bytes to spare) ...
    fs.rename(d, b"f", d, b"123456789a").unwrap();
    assert_eq!(fs.lookup(d, b"123456789a"), Ok(f));
    // ... one more byte does not
    assert_eq!(
        fs.rename(d, b"123456789a", d, b"123456789ab"),
        Err(Error::Quota)
    );
    assert_eq!(
        fs.lookup(d, b"123456789a"),
        Ok(f),
        "a refused rename changes nothing"
    );
    // shrinking gives the bytes back
    fs.rename(d, b"123456789a", d, b"f").unwrap();
    fs.rename(d, b"f", d, b"123456789a").unwrap();
}
