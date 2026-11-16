//! Session profiles (USER-VIEW-SPEC section 10). **A user is a profile**: a table template, a home, a shell, the values the
//! libc synthesizes for `uid`, `gid` and `getpwuid`, the programs that get more than the default confined table, and quotas.
//! Nothing in the kernel or any server knows about users; the session manager reads a profile and builds a session from it.
//!
//! The format is one strict JSON object (parsed by the `data` crate under its limits). Unknown keys are an error: a typo in
//! a profile must not silently drop a grant or a limit.
//!
//! ```json
//! { "name": "alice", "uid": 1000, "gid": 1000,
//!   "home": "homes/alice",
//!   "shell": "sh",
//!   "env": { "EDITOR": "kilo" },
//!   "programs": { "ls": { "root": true, "mounts": [ { "path": "/mnt/store", "volume": "store", "rights": "rw" } ] } },
//!   "quota": { "memory_mib": 64, "processes": 32 },
//!   "autologin": true }
//! ```
//!
//! `home` names the directory of the boot image that is `/home` in the session (until home volumes exist), `programs` replaces
//! `/etc/trusted`: a program listed gets a read-only root if `root` is true and the listed volumes at the listed paths.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use data::{Limits, Value};

pub const MAX_PROFILE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rights {
    Read,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// Where it appears in the program's table (`/mnt/store`).
    pub path: String,
    /// The volume's name in the mount table.
    pub volume: String,
    pub rights: Rights,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grant {
    /// Gets a read-only root (`/`).
    pub root: bool,
    pub mounts: Vec<Mount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quota {
    pub memory_mib: u64,
    pub processes: u64,
}

impl Default for Quota {
    fn default() -> Quota {
        Quota {
            memory_mib: 64,
            processes: 32,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The boot-image directory that is `/home` in the session.
    pub home: String,
    /// The program started as the session's shell (`bin/<shell>`).
    pub shell: String,
    pub env: Vec<(String, String)>,
    pub programs: Vec<(String, Grant)>,
    /// Limits for the programs of the session; `None` leaves the system's defaults.
    pub quota: Option<Quota>,
    pub autologin: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    TooLarge,
    /// Not valid JSON, or not an object.
    Syntax(String),
    UnknownKey(String),
    Missing(&'static str),
    /// A value of the wrong kind or out of range: (key, why).
    Bad(String, &'static str),
}

impl core::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ProfileError::TooLarge => write!(f, "profile too large"),
            ProfileError::Syntax(m) => write!(f, "profile is not valid JSON: {m}"),
            ProfileError::UnknownKey(k) => write!(f, "unknown profile key `{k}`"),
            ProfileError::Missing(k) => write!(f, "profile lacks `{k}`"),
            ProfileError::Bad(k, why) => write!(f, "profile `{k}`: {why}"),
        }
    }
}

/// A profile or program name: lower-case letters, digits, `-` and `_`, starting with a letter, at most 32 long.
pub fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-' || *c == b'_')
}

/// A path in the image or a table: absolute (`/mnt/x`) or relative (`homes/alice`), components of printable non-slash text
/// that are not empty, `.` or `..`.
fn valid_path_components(s: &str) -> bool {
    let body = s.strip_prefix('/').unwrap_or(s);
    !body.is_empty()
        && body
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != ".." && c.chars().all(|ch| !ch.is_control()))
}

fn str_of<'a>(key: &str, v: &'a Value) -> Result<&'a str, ProfileError> {
    match v {
        Value::Str(s) => Ok(s),
        _ => Err(ProfileError::Bad(String::from(key), "must be a string")),
    }
}

fn uint_of(key: &str, v: &Value, max: u64) -> Result<u64, ProfileError> {
    match v {
        Value::Num(data::Number::Int(i)) if *i >= 0 && (*i as u64) <= max => Ok(*i as u64),
        _ => Err(ProfileError::Bad(
            String::from(key),
            "must be a whole number in range",
        )),
    }
}

fn parse_rights(key: &str, v: &Value) -> Result<Rights, ProfileError> {
    match str_of(key, v)? {
        "r" => Ok(Rights::Read),
        "rw" => Ok(Rights::ReadWrite),
        _ => Err(ProfileError::Bad(
            String::from(key),
            "must be \"r\" or \"rw\"",
        )),
    }
}

fn only_keys(m: &data::Map, allowed: &[&str]) -> Result<(), ProfileError> {
    for k in m.keys() {
        if !allowed.contains(&k) {
            return Err(ProfileError::UnknownKey(String::from(k)));
        }
    }
    Ok(())
}

impl Profile {
    pub fn parse(text: &str) -> Result<Profile, ProfileError> {
        if text.len() > MAX_PROFILE_BYTES {
            return Err(ProfileError::TooLarge);
        }
        let lim = Limits {
            max_depth: 8,
            max_bytes: MAX_PROFILE_BYTES,
            max_nodes: 4096,
        };
        let v = data::from_json(text.as_bytes(), &lim)
            .map_err(|e| ProfileError::Syntax(alloc::format!("{e}")))?;
        let Value::Map(m) = v else {
            return Err(ProfileError::Syntax(String::from("not an object")));
        };
        only_keys(
            &m,
            &[
                "name",
                "uid",
                "gid",
                "home",
                "shell",
                "env",
                "programs",
                "quota",
                "autologin",
            ],
        )?;
        let name = str_of("name", m.get("name").ok_or(ProfileError::Missing("name"))?)?;
        if !valid_name(name) {
            return Err(ProfileError::Bad(
                String::from("name"),
                "lower-case letters, digits, - and _, starting with a letter",
            ));
        }
        let uid = uint_of(
            "uid",
            m.get("uid").ok_or(ProfileError::Missing("uid"))?,
            u32::MAX as u64,
        )? as u32;
        let gid = match m.get("gid") {
            Some(g) => uint_of("gid", g, u32::MAX as u64)? as u32,
            None => uid,
        };
        let home = str_of("home", m.get("home").ok_or(ProfileError::Missing("home"))?)?;
        if home.starts_with('/') || !valid_path_components(home) {
            return Err(ProfileError::Bad(
                String::from("home"),
                "a relative image path such as homes/alice",
            ));
        }
        let shell = match m.get("shell") {
            Some(s) => str_of("shell", s)?,
            None => "sh",
        };
        if !valid_name(shell) {
            return Err(ProfileError::Bad(String::from("shell"), "a program name"));
        }
        let mut env = Vec::new();
        if let Some(e) = m.get("env") {
            let Value::Map(em) = e else {
                return Err(ProfileError::Bad(String::from("env"), "must be an object"));
            };
            for (k, v) in em.iter() {
                let ok = !k.is_empty()
                    && k.chars()
                        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                if !ok
                    || matches!(
                        k,
                        "PWD" | "STDIN" | "STDOUT" | "USER" | "HOME" | "UID" | "GID"
                    )
                {
                    return Err(ProfileError::Bad(
                        String::from("env"),
                        "names are upper case and not ones the session sets itself",
                    ));
                }
                env.push((String::from(k), String::from(str_of("env", v)?)));
            }
        }
        let mut programs = Vec::new();
        if let Some(p) = m.get("programs") {
            let Value::Map(pm) = p else {
                return Err(ProfileError::Bad(
                    String::from("programs"),
                    "must be an object",
                ));
            };
            for (prog, g) in pm.iter() {
                if !valid_name(prog) {
                    return Err(ProfileError::Bad(
                        String::from("programs"),
                        "a program name",
                    ));
                }
                let Value::Map(gm) = g else {
                    return Err(ProfileError::Bad(
                        String::from("programs"),
                        "each program is an object",
                    ));
                };
                only_keys(gm, &["root", "mounts"])?;
                let mut grant = Grant::default();
                if let Some(r) = gm.get("root") {
                    match r {
                        Value::Bool(b) => grant.root = *b,
                        _ => return Err(ProfileError::Bad(String::from("root"), "true or false")),
                    }
                }
                if let Some(ms) = gm.get("mounts") {
                    let Value::List(list) = ms else {
                        return Err(ProfileError::Bad(String::from("mounts"), "must be a list"));
                    };
                    for item in list {
                        let Value::Map(mm) = item else {
                            return Err(ProfileError::Bad(
                                String::from("mounts"),
                                "each mount is an object",
                            ));
                        };
                        only_keys(mm, &["path", "volume", "rights"])?;
                        let volume = str_of(
                            "volume",
                            mm.get("volume").ok_or(ProfileError::Missing("volume"))?,
                        )?;
                        if !valid_name(volume) {
                            return Err(ProfileError::Bad(String::from("volume"), "a volume name"));
                        }
                        let path = match mm.get("path") {
                            Some(p) => String::from(str_of("path", p)?),
                            None => alloc::format!("/mnt/{volume}"),
                        };
                        if !path.starts_with('/') || path == "/" || !valid_path_components(&path) {
                            return Err(ProfileError::Bad(
                                String::from("path"),
                                "an absolute path below /",
                            ));
                        }
                        let rights = match mm.get("rights") {
                            Some(r) => parse_rights("rights", r)?,
                            None => Rights::Read,
                        };
                        grant.mounts.push(Mount {
                            path,
                            volume: String::from(volume),
                            rights,
                        });
                    }
                }
                programs.push((String::from(prog), grant));
            }
        }
        let mut quota = None;
        if let Some(q) = m.get("quota") {
            let mut qq = Quota::default();
            let Value::Map(qm) = q else {
                return Err(ProfileError::Bad(
                    String::from("quota"),
                    "must be an object",
                ));
            };
            only_keys(qm, &["memory_mib", "processes"])?;
            if let Some(v) = qm.get("memory_mib") {
                let n = uint_of("memory_mib", v, 1 << 20)?;
                if n == 0 {
                    return Err(ProfileError::Bad(String::from("memory_mib"), "at least 1"));
                }
                qq.memory_mib = n;
            }
            if let Some(v) = qm.get("processes") {
                let n = uint_of("processes", v, 4096)?;
                if n == 0 {
                    return Err(ProfileError::Bad(String::from("processes"), "at least 1"));
                }
                qq.processes = n;
            }
            quota = Some(qq);
        }
        let autologin = match m.get("autologin") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                return Err(ProfileError::Bad(
                    String::from("autologin"),
                    "true or false",
                ))
            }
        };
        Ok(Profile {
            name: String::from(name),
            uid,
            gid,
            home: String::from(home),
            shell: String::from(shell),
            env,
            programs,
            quota,
            autologin,
        })
    }

    /// The grant for a program (none if it is not listed: it gets the default confined table).
    pub fn grant(&self, program: &str) -> Option<&Grant> {
        self.programs
            .iter()
            .find(|(p, _)| p == program)
            .map(|(_, g)| g)
    }

    /// The environment every program of the session starts with, in order: the profile's own, then the identity the libc
    /// synthesizes `getpwuid` from.
    pub fn session_env(&self) -> Vec<(String, String)> {
        let mut v = self.env.clone();
        v.push((String::from("USER"), self.name.clone()));
        v.push((String::from("HOME"), String::from("/home")));
        v.push((String::from("UID"), alloc::format!("{}", self.uid)));
        v.push((String::from("GID"), alloc::format!("{}", self.gid)));
        v
    }

    /// The `/etc/passwd` line the libc would synthesize (a stand-in until the session manager serves the file).
    pub fn passwd_line(&self) -> String {
        alloc::format!(
            "{}:x:{}:{}:{}:/home:/bin/{}\n",
            self.name,
            self.uid,
            self.gid,
            self.name,
            self.shell
        )
    }
}

/// The profile that is logged in without asking: the single one marked `autologin` (an error if there are two, so that
/// "who is logged in at boot" is never a matter of file order).
pub fn pick_autologin(profiles: &[Profile]) -> Result<Option<&Profile>, ProfileError> {
    let mut it = profiles.iter().filter(|p| p.autologin);
    let first = it.next();
    if it.next().is_some() {
        return Err(ProfileError::Bad(
            String::from("autologin"),
            "more than one profile is marked",
        ));
    }
    Ok(first)
}

/// The boot-image directories that are some profile's home must be distinct, so one profile's home is never another's.
pub fn check_homes_distinct(profiles: &[Profile]) -> Result<(), ProfileError> {
    for (i, a) in profiles.iter().enumerate() {
        for b in &profiles[i + 1..] {
            let (x, y) = (a.home.trim_end_matches('/'), b.home.trim_end_matches('/'));
            let nested = x == y
                || x.starts_with(&alloc::format!("{y}/"))
                || y.starts_with(&alloc::format!("{x}/"));
            if nested {
                return Err(ProfileError::Bad(
                    String::from("home"),
                    "two profiles share a home (or one is inside the other)",
                ));
            }
        }
    }
    Ok(())
}
