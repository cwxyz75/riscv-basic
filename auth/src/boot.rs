//! What a boot does about logging in (USER-VIEW-SPEC section 10.1): a pure decision from the facts the boot has, and the
//! one-shot setup that creates the first profile. Nothing here touches a service; `init` and `authd` act on the answer.
//!
//! The rules, one sentence each:
//! * With no profile on the volume only the **setup session** may start; it can create one profile and then it no longer
//!   exists (`Setup::create` works once).
//! * **Autologin** is used only when the manifest enables it *and* a profile is marked for it; a manifest that does not
//!   enable it never logs anyone in without a credential, whatever the profiles say.
//! * Otherwise a boot with a credential store waits for a login through `authd`: a right password yields a session for
//!   that profile only.
//! * **Recovery** (resetting a credential) is honoured only when the physical console asked for it at this boot.

use alloc::string::String;

/// What the boot knows when it decides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts<'a> {
    /// Names of the profiles on the volume.
    pub profiles: &'a [&'a str],
    /// The profile marked `autologin`, if exactly one is.
    pub autologin_profile: Option<&'a str>,
    /// The manifest enables autologin (development manifests only).
    pub autologin_enabled: bool,
    /// How many credentials `authd` holds (a seed credential counts).
    pub credentials: usize,
    /// The physical console asked for recovery at this boot.
    pub recovery_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// No profile exists: start the setup session and nothing else.
    Setup,
    /// Start this profile's session without a credential (logged).
    Autologin(String),
    /// Wait for a login through `authd`.
    Login,
    /// A login through `authd`, and the console may reset a credential (recovery).
    LoginWithRecovery,
    /// A profile exists but nobody can ever log in (no credential, no autologin): refuse to start, loudly.
    Locked,
}

pub fn decide(f: &Facts) -> Plan {
    if f.profiles.is_empty() {
        return Plan::Setup;
    }
    if f.autologin_enabled {
        if let Some(p) = f.autologin_profile {
            if f.profiles.contains(&p) {
                return Plan::Autologin(String::from(p));
            }
        }
    }
    if f.credentials == 0 && !f.recovery_requested {
        return Plan::Locked;
    }
    if f.recovery_requested {
        Plan::LoginWithRecovery
    } else {
        Plan::Login
    }
}

/// May a credential be reset now? Only when the physical console asked at this boot; no credential can authorize it.
pub fn may_reset_credential(recovery_requested: bool) -> bool {
    recovery_requested
}

/// The setup session's single power: create the first profile.
#[derive(Debug, Default)]
pub struct Setup {
    done: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The first profile already exists: the setup session has ceased to exist.
    Gone,
    BadName,
}

impl Setup {
    pub fn new() -> Setup {
        Setup { done: false }
    }

    /// The name of the profile to create, or why not. After one success this is always `Gone`.
    pub fn create(&mut self, name: &str) -> Result<String, SetupError> {
        if self.done {
            return Err(SetupError::Gone);
        }
        let ok = !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
        if !ok {
            return Err(SetupError::BadName);
        }
        self.done = true;
        Ok(String::from(name))
    }

    pub fn exists(&self) -> bool {
        !self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(profiles: &'a [&'a str]) -> Facts<'a> {
        Facts {
            profiles,
            autologin_profile: None,
            autologin_enabled: false,
            credentials: 0,
            recovery_requested: false,
        }
    }

    #[test]
    fn an_empty_volume_gets_only_the_setup_session() {
        // even with autologin enabled and credentials claimed: nothing to log in to
        let mut f = facts(&[]);
        f.autologin_enabled = true;
        f.credentials = 3;
        assert_eq!(decide(&f), Plan::Setup);
    }

    #[test]
    fn setup_creates_one_profile_then_is_gone() {
        let mut s = Setup::new();
        assert!(s.exists());
        assert_eq!(s.create("alice"), Ok(String::from("alice")));
        assert!(!s.exists());
        assert_eq!(s.create("bob"), Err(SetupError::Gone));
        assert_eq!(s.create("alice"), Err(SetupError::Gone));
    }

    #[test]
    fn setup_refuses_bad_names_without_using_itself_up() {
        let mut s = Setup::new();
        for bad in ["", "Al", "a b", "../x", "a/b", &"x".repeat(33)] {
            assert_eq!(s.create(bad), Err(SetupError::BadName), "{bad:?}");
        }
        assert!(s.exists());
        assert!(s.create("ok-name_1").is_ok());
    }

    #[test]
    fn autologin_needs_the_manifest_and_a_marked_profile() {
        let mut f = facts(&["alice", "bob"]);
        f.autologin_profile = Some("alice");
        f.credentials = 1;
        // the manifest does not enable it: a login is required
        assert_eq!(decide(&f), Plan::Login);
        f.autologin_enabled = true;
        assert_eq!(decide(&f), Plan::Autologin(String::from("alice")));
        // marked profile missing from the volume
        f.autologin_profile = Some("carol");
        assert_eq!(decide(&f), Plan::Login);
        // nobody marked
        f.autologin_profile = None;
        assert_eq!(decide(&f), Plan::Login);
    }

    #[test]
    fn a_manifest_without_autologin_and_no_credential_is_locked_not_open() {
        let mut f = facts(&["alice"]);
        f.autologin_profile = Some("alice");
        assert_eq!(decide(&f), Plan::Locked);
    }

    #[test]
    fn recovery_only_when_the_console_asked() {
        let mut f = facts(&["alice"]);
        f.credentials = 1;
        assert_eq!(decide(&f), Plan::Login);
        f.recovery_requested = true;
        assert_eq!(decide(&f), Plan::LoginWithRecovery);
        // a lost credential store is recoverable by the console
        f.credentials = 0;
        assert_eq!(decide(&f), Plan::LoginWithRecovery);
        assert!(may_reset_credential(true));
        assert!(!may_reset_credential(false));
    }
}
