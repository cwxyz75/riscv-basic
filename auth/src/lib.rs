//! Credential verifiers (USER-VIEW-SPEC section 10.1, step UV5a). `authd` holds these and nothing else; the session manager
//! never sees one. A verifier is a memory-hard hash (scrypt) of the password with a per-credential salt.
//!
//! Text form, one per credential: `scrypt$<log2 N>$<r>$<p>$<salt hex>$<hash hex>`. The store is a text file of lines
//! `<profile name> <verifier>`; blank lines and `#` comments are ignored; a name may appear once.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod boot;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

pub const SALT_LEN: usize = 16;
pub const HASH_LEN: usize = 32;
/// Parameters above these are refused when parsing: a hostile record must not make `authd` allocate gigabytes.
pub const MAX_LOG_N: u8 = 16;
pub const MAX_R: u32 = 16;
pub const MAX_P: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub log_n: u8,
    pub r: u32,
    pub p: u32,
}

impl Params {
    /// What a real system uses (16 MiB of working memory per verification).
    pub const STRONG: Params = Params {
        log_n: 14,
        r: 8,
        p: 1,
    };
    /// For tests and the seed credential of a development image: cheap, and good for nothing else.
    pub const TEST: Params = Params {
        log_n: 4,
        r: 8,
        p: 1,
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Malformed,
    TooCostly,
    BadParams,
    DuplicateName,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Verifier {
    pub params: Params,
    pub salt: [u8; SALT_LEN],
    pub hash: [u8; HASH_LEN],
}

// (no `Debug` that prints the hash: a verifier does not belong in a log)
impl core::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Verifier(scrypt, log_n {})", self.params.log_n)
    }
}

fn derive(password: &[u8], salt: &[u8], params: Params) -> Result<[u8; HASH_LEN], Error> {
    if params.log_n == 0
        || params.log_n > MAX_LOG_N
        || params.r == 0
        || params.r > MAX_R
        || params.p == 0
        || params.p > MAX_P
    {
        return Err(Error::TooCostly);
    }
    let p = scrypt::Params::new(params.log_n, params.r, params.p, HASH_LEN)
        .map_err(|_| Error::BadParams)?;
    let mut out = [0u8; HASH_LEN];
    scrypt::scrypt(password, salt, &p, &mut out).map_err(|_| Error::BadParams)?;
    Ok(out)
}

/// Equal without stopping at the first difference.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b) {
        d |= x ^ y;
    }
    d == 0
}

impl Verifier {
    /// Make a verifier from a password and a salt the caller drew from the random service.
    pub fn new(password: &[u8], salt: [u8; SALT_LEN], params: Params) -> Result<Verifier, Error> {
        let hash = derive(password, &salt, params)?;
        Ok(Verifier { params, salt, hash })
    }

    pub fn verify(&self, password: &[u8]) -> bool {
        match derive(password, &self.salt, self.params) {
            Ok(h) => ct_eq(&h, &self.hash),
            Err(_) => false,
        }
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        let _ = write!(
            s,
            "scrypt${}${}${}$",
            self.params.log_n, self.params.r, self.params.p
        );
        hex(&mut s, &self.salt);
        s.push('$');
        hex(&mut s, &self.hash);
        s
    }

    pub fn parse(text: &str) -> Result<Verifier, Error> {
        let mut it = text.split('$');
        if it.next() != Some("scrypt") {
            return Err(Error::Malformed);
        }
        let log_n: u8 = it
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(Error::Malformed)?;
        let r: u32 = it
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(Error::Malformed)?;
        let p: u32 = it
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or(Error::Malformed)?;
        let salt = it
            .next()
            .and_then(unhex::<SALT_LEN>)
            .ok_or(Error::Malformed)?;
        let hash = it
            .next()
            .and_then(unhex::<HASH_LEN>)
            .ok_or(Error::Malformed)?;
        if it.next().is_some() {
            return Err(Error::Malformed);
        }
        if log_n == 0 || log_n > MAX_LOG_N || r == 0 || r > MAX_R || p == 0 || p > MAX_P {
            return Err(Error::TooCostly);
        }
        Ok(Verifier {
            params: Params { log_n, r, p },
            salt,
            hash,
        })
    }
}

fn hex(s: &mut String, bytes: &[u8]) {
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
}

fn unhex<const N: usize>(t: &str) -> Option<[u8; N]> {
    let b = t.as_bytes();
    if b.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        let d = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        };
        out[i] = d(b[2 * i])? << 4 | d(b[2 * i + 1])?;
    }
    Some(out)
}

/// The credential store: which profile names have a verifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Store {
    entries: Vec<(String, Verifier)>,
}

impl Store {
    pub fn parse(text: &str) -> Result<Store, Error> {
        let mut s = Store::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (name, v) = line.split_once(' ').ok_or(Error::Malformed)?;
            if name.is_empty() || name.contains(char::is_whitespace) {
                return Err(Error::Malformed);
            }
            let v = Verifier::parse(v.trim())?;
            if s.entries.iter().any(|(n, _)| n == name) {
                return Err(Error::DuplicateName);
            }
            s.entries.push((String::from(name), v));
        }
        Ok(s)
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for (n, v) in &self.entries {
            out.push_str(n);
            out.push(' ');
            out.push_str(&v.to_text());
            out.push('\n');
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(n, _)| n.as_str())
    }

    /// Set (or replace) a profile's verifier.
    pub fn set(&mut self, name: &str, v: Verifier) {
        match self.entries.iter_mut().find(|(n, _)| n == name) {
            Some(e) => e.1 = v,
            None => self.entries.push((String::from(name), v)),
        }
    }

    /// May this password obtain a session for `name`? An unknown name does the same work as a wrong password (no timing
    /// difference that tells the two apart).
    pub fn check(&self, name: &str, password: &[u8]) -> bool {
        match self.entries.iter().find(|(n, _)| n == name) {
            Some((_, v)) => v.verify(password),
            None => {
                // the same work as a real check, at the cost the first entry has
                let params = self.entries.first().map_or(Params::TEST, |(_, v)| v.params);
                let _ = derive(password, &[0u8; SALT_LEN], params);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    const SALT: [u8; SALT_LEN] = [7; SALT_LEN];

    #[test]
    fn rfc7914_vector() {
        // RFC 7914 section 12, first vector (N=16, r=1, p=1), first 32 of its 64 bytes
        let got = derive(
            b"",
            b"",
            Params {
                log_n: 4,
                r: 1,
                p: 1,
            },
        )
        .unwrap();
        let mut s = String::new();
        hex(&mut s, &got);
        assert_eq!(
            s,
            "77d6576238657b203b19ca42c18a0497f16b4844e3074ae8dfdffa3fede21442"
        );
    }

    #[test]
    fn right_password_passes_and_wrong_ones_fail() {
        let v = Verifier::new(b"correct horse", SALT, Params::TEST).unwrap();
        assert!(v.verify(b"correct horse"));
        assert!(!v.verify(b"correct hors"));
        assert!(!v.verify(b"correct horsex"));
        assert!(!v.verify(b""));
    }

    #[test]
    fn the_salt_matters() {
        let a = Verifier::new(b"pw", [1; SALT_LEN], Params::TEST).unwrap();
        let b = Verifier::new(b"pw", [2; SALT_LEN], Params::TEST).unwrap();
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn text_round_trip() {
        let v = Verifier::new(b"pw", SALT, Params::TEST).unwrap();
        assert_eq!(Verifier::parse(&v.to_text()).unwrap(), v);
    }

    #[test]
    fn malformed_records_are_refused() {
        let v = Verifier::new(b"pw", SALT, Params::TEST).unwrap().to_text();
        for bad in [
            "",
            "argon2$4$8$1$00$00",
            &v.replace("scrypt", "scrpyt"),
            &v[..v.len() - 2],
            &format!("{v}$x"),
            &v.replace("$4$", "$x$"),
            &v.replace(&hex_of(&SALT), &"zz".repeat(SALT_LEN)),
        ] {
            assert_eq!(Verifier::parse(bad), Err(Error::Malformed), "{bad}");
        }
    }

    fn hex_of(b: &[u8]) -> String {
        let mut s = String::new();
        hex(&mut s, b);
        s
    }

    #[test]
    fn costly_parameters_are_refused_not_run() {
        let v = Verifier::new(b"pw", SALT, Params::TEST).unwrap().to_text();
        assert_eq!(
            Verifier::parse(&v.replace("$4$8$1$", "$30$8$1$")),
            Err(Error::TooCostly)
        );
        assert_eq!(
            Verifier::parse(&v.replace("$4$8$1$", "$4$8$9$")),
            Err(Error::TooCostly)
        );
        assert_eq!(
            Verifier::parse(&v.replace("$4$8$1$", "$0$8$1$")),
            Err(Error::TooCostly)
        );
        assert!(Verifier::new(
            b"pw",
            SALT,
            Params {
                log_n: 30,
                r: 8,
                p: 1
            }
        )
        .is_err());
    }

    #[test]
    fn store_checks_by_name() {
        let mut s = Store::default();
        s.set(
            "alice",
            Verifier::new(b"a-pass", SALT, Params::TEST).unwrap(),
        );
        s.set("bob", Verifier::new(b"b-pass", SALT, Params::TEST).unwrap());
        assert!(s.check("alice", b"a-pass"));
        assert!(!s.check("alice", b"b-pass"));
        assert!(s.check("bob", b"b-pass"));
        assert!(!s.check("carol", b"a-pass"));
        assert!(!s.check("", b""));
    }

    #[test]
    fn store_round_trip_and_rules() {
        let mut s = Store::default();
        s.set("alice", Verifier::new(b"x", SALT, Params::TEST).unwrap());
        s.set("alice", Verifier::new(b"y", SALT, Params::TEST).unwrap()); // replaces
        assert_eq!(s.names().count(), 1);
        assert!(s.check("alice", b"y") && !s.check("alice", b"x"));
        let text = s.to_text();
        assert_eq!(Store::parse(&format!("# note\n\n{text}")).unwrap(), s);
        assert_eq!(
            Store::parse(&format!("{text}{text}")),
            Err(Error::DuplicateName)
        );
        assert_eq!(Store::parse("alice"), Err(Error::Malformed));
        assert!(Store::parse("").unwrap().is_empty());
    }

    #[test]
    fn a_verifier_does_not_print_its_hash() {
        let v = Verifier::new(b"pw", SALT, Params::TEST).unwrap();
        let shown = format!("{v:?}");
        assert!(!shown.contains(&hex_of(&v.hash)));
    }
}
