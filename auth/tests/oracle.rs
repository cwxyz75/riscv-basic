//! Independent oracle: Python's `hashlib.scrypt` (OpenSSL) computes the same hashes. Skipped when `python3` is absent or
//! its OpenSSL lacks scrypt.

use auth::{Params, Verifier};
use std::process::Command;

fn python_scrypt(password: &str, salt: &[u8], log_n: u8, r: u32, p: u32) -> Option<String> {
    let salt_hex: String = salt.iter().map(|b| format!("{b:02x}")).collect();
    let code = format!(
        "import hashlib,sys\nprint(hashlib.scrypt({:?}.encode(), salt=bytes.fromhex('{salt_hex}'), n=1<<{log_n}, r={r}, p={p}, dklen=32).hex())",
        password
    );
    let out = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(code)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_string())
}

#[test]
fn hashes_match_python() {
    let mut checked = 0;
    for (pw, salt, log_n, r, p) in [
        ("hunter2", [0u8; 16], 4u8, 8u32, 1u32),
        ("correct horse battery staple", [0xab; 16], 6, 8, 1),
        ("", [1; 16], 5, 4, 2),
        ("pässwörd ✓", [9; 16], 10, 8, 1),
    ] {
        let Some(want) = python_scrypt(pw, &salt, log_n, r, p) else {
            eprintln!("python3 with scrypt is not available: skipped");
            return;
        };
        let v = Verifier::new(pw.as_bytes(), salt, Params { log_n, r, p }).unwrap();
        let got: String = v.hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, want, "{pw:?}");
        checked += 1;
    }
    assert_eq!(checked, 4);
}
