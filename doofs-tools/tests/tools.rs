use std::process::Command;

fn bin(name: &str) -> String {
    let exe = env!("CARGO_BIN_EXE_mkfs-doofs");
    exe.replace("mkfs-doofs", name)
}

fn img(tag: &str) -> String {
    let p = std::env::temp_dir().join(format!("doofs-tools-{}-{tag}.img", std::process::id()));
    p.to_string_lossy().into_owned()
}

#[test]
fn mkfs_then_fsck_and_dump() {
    let p = img("a");
    let out = Command::new(bin("mkfs-doofs"))
        .args([
            "--uuid",
            "00112233-4455-6677-8899-aabbccddeeff",
            "--label",
            "test",
            "--size-mib",
            "2",
            &p,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let fsck = Command::new(bin("fsck-doofs")).arg(&p).output().unwrap();
    let text = String::from_utf8_lossy(&fsck.stdout);
    assert!(fsck.status.success(), "{text}");
    assert!(text.contains("mount uses slot 1"), "{text}");
    assert!(text.contains("00112233445566778899aabbccddeeff"), "{text}");
    let dump = Command::new(bin("dump-doofs"))
        .args([&p, "--tree"])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&dump.stdout);
    assert!(dump.status.success());
    assert!(text.contains("label \"test\" generation 1"), "{text}");
    std::fs::remove_file(&p).unwrap();
}

#[test]
fn fsck_fails_on_garbage_and_missing_files() {
    let p = img("b");
    std::fs::write(&p, vec![0x5au8; 64 * 4096]).unwrap();
    let r = Command::new(bin("fsck-doofs")).arg(&p).output().unwrap();
    assert_eq!(r.status.code(), Some(1));
    let r = Command::new(bin("fsck-doofs"))
        .arg("/nonexistent/image")
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(2));
    std::fs::remove_file(&p).unwrap();
}
