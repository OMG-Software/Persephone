//! Local profiles (accounts) side by side on one computer (ADR-0015).
//!
//! Each account has its own encrypted profile: its own stores, key and
//! passphrase. Slot 1 is the base profile, the default one or the directory
//! given with `--profile`. Slot `n` (2 to [`MAX_PROFILES`]) is the sibling
//! directory named like the base with `-n` added, for example `profile-2`.
//!
//! * Only slot numbers cross to the UI: never a path, and nothing about the
//!   account. Nothing is stored outside the profile's encrypted store to
//!   name it, so the UI shows generic labels.
//! * One profile is open per core process. The core never reopens or hands
//!   over stores in the same process after a client ran (ADR-0008), so a
//!   switch closes the window, locks the stores as exit does, starts a new
//!   core for the chosen slot, and exits.

use persephone_contracts::{ProfileList, ProfileSlot};
use persephone_core_protocol::{ErrorCode, MAX_PROFILES};
use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

/// The core option that names a slot other than 1.
pub const ACCOUNT_OPTION: &str = "--account";

/// The profile directory of `slot` beside `base`, or `None` for a slot out
/// of range or a base without a file name.
pub fn dir_of(base: &Path, slot: u32) -> Option<PathBuf> {
    if !(1..=MAX_PROFILES).contains(&slot) {
        return None;
    }
    if slot == 1 {
        return Some(base.to_owned());
    }
    let mut name = base.file_name()?.to_owned();
    name.push(format!("-{slot}"));
    Some(base.with_file_name(name))
}

/// The profiles beside the base directory, and the slot open now.
pub struct Profiles {
    base: PathBuf,
    active: u32,
    /// The base directory as the core was given it, for the new core.
    given: Option<PathBuf>,
    /// A switch the UI asked for, waiting for the window to close.
    switch: Mutex<Option<u32>>,
}

impl Profiles {
    /// `base` is the slot 1 directory; `given` is the `--profile` directory,
    /// if one was given, so the new core opens the same base.
    pub fn new(base: PathBuf, given: Option<PathBuf>, active: u32) -> Option<Self> {
        dir_of(&base, active)?;
        Some(Self {
            base,
            active,
            given,
            switch: Mutex::new(None),
        })
    }

    pub fn active(&self) -> u32 {
        self.active
    }

    /// The open profile's directory.
    pub fn dir(&self) -> PathBuf {
        dir_of(&self.base, self.active).expect("checked in new")
    }

    fn exists(&self, slot: u32) -> bool {
        dir_of(&self.base, slot).is_some_and(|dir| dir.is_dir())
    }

    /// The profiles that exist, and the open one, in slot order.
    pub fn list(&self) -> ProfileList {
        let profiles = (1..=MAX_PROFILES)
            .filter(|&slot| slot == self.active || self.exists(slot))
            .map(|slot| ProfileSlot {
                slot,
                active: slot == self.active,
            })
            .collect();
        ProfileList {
            profiles,
            can_add: self.free_slot().is_some(),
        }
    }

    /// The lowest slot with no profile yet.
    fn free_slot(&self) -> Option<u32> {
        (1..=MAX_PROFILES).find(|&slot| slot != self.active && !self.exists(slot))
    }

    /// Asks to switch to a new profile in the lowest free slot.
    /// `limit_exceeded` when every slot is taken or a switch is waiting.
    pub fn request_new(&self) -> Result<u32, ErrorCode> {
        let slot = self.free_slot().ok_or(ErrorCode::LimitExceeded)?;
        self.request(slot)?;
        Ok(slot)
    }

    /// Asks to switch to the existing profile in `slot`. The open slot, or
    /// one out of range, is `invalid_argument`; an empty slot is
    /// `not_found`; a waiting switch is `limit_exceeded`.
    pub fn request_switch(&self, slot: u32) -> Result<(), ErrorCode> {
        if slot == self.active || dir_of(&self.base, slot).is_none() {
            return Err(ErrorCode::InvalidArgument);
        }
        if !self.exists(slot) {
            return Err(ErrorCode::NotFound);
        }
        self.request(slot)
    }

    fn request(&self, slot: u32) -> Result<(), ErrorCode> {
        let mut switch = self
            .switch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if switch.is_some() {
            return Err(ErrorCode::LimitExceeded);
        }
        *switch = Some(slot);
        Ok(())
    }

    /// The slot the UI asked to switch to, if any.
    pub fn requested(&self) -> Option<u32> {
        *self
            .switch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The arguments that start a core with `slot` open.
    pub fn arguments(&self, slot: u32) -> Vec<OsString> {
        let mut args = Vec::new();
        if let Some(given) = &self.given {
            args.push(OsString::from("--profile"));
            args.push(given.clone().into_os_string());
        }
        if slot != 1 {
            args.push(OsString::from(ACCOUNT_OPTION));
            args.push(OsString::from(slot.to_string()));
        }
        args
    }

    /// Starts a new core, this executable, with `slot` open. Called last,
    /// after this core's stores are locked; the new core asks for that
    /// profile's passphrase in its own window.
    pub fn start_core(&self, slot: u32) -> io::Result<()> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(self.arguments(slot))
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        crate::platform::os::start_detached(&mut command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("persephone-profiles-{}-{name}", std::process::id()))
            .join("profile");
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        dir
    }

    /// GIVEN a base directory THEN slot 1 is the base, slot n its sibling
    /// with `-n`, and slots out of range have no directory.
    #[test]
    fn given_a_base_then_slots_are_siblings_with_a_number() {
        let base = Path::new("data").join("profile");
        assert_eq!(dir_of(&base, 1), Some(base.clone()));
        assert_eq!(dir_of(&base, 2), Some(Path::new("data").join("profile-2")));
        assert_eq!(
            dir_of(&base, MAX_PROFILES),
            Some(Path::new("data").join(format!("profile-{MAX_PROFILES}")))
        );
        assert_eq!(dir_of(&base, 0), None);
        assert_eq!(dir_of(&base, MAX_PROFILES + 1), None);
        assert!(Profiles::new(base, None, 0).is_none());
    }

    /// GIVEN profiles in slots 1 and 3 WHEN listed THEN both appear, the open
    /// one is marked, and a new profile takes slot 2, the lowest free one.
    #[test]
    fn given_profiles_then_the_list_and_the_next_free_slot_follow_the_disk() {
        let base = temp_base("list");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(dir_of(&base, 3).unwrap()).unwrap();
        let profiles = Profiles::new(base.clone(), None, 3).unwrap();
        assert_eq!(
            profiles.list(),
            ProfileList {
                profiles: vec![
                    ProfileSlot {
                        slot: 1,
                        active: false
                    },
                    ProfileSlot {
                        slot: 3,
                        active: true
                    },
                ],
                can_add: true,
            }
        );
        assert_eq!(profiles.request_new(), Ok(2));
        assert_eq!(profiles.requested(), Some(2));
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    /// GIVEN switch requests WHEN made THEN the open slot, an empty slot and
    /// one out of range are refused, and only one switch may wait.
    #[test]
    fn given_switch_requests_then_only_one_existing_other_slot_is_accepted() {
        let base = temp_base("switch");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(dir_of(&base, 2).unwrap()).unwrap();
        let profiles = Profiles::new(base.clone(), None, 1).unwrap();
        assert_eq!(profiles.request_switch(1), Err(ErrorCode::InvalidArgument));
        assert_eq!(
            profiles.request_switch(MAX_PROFILES + 1),
            Err(ErrorCode::InvalidArgument)
        );
        assert_eq!(profiles.request_switch(4), Err(ErrorCode::NotFound));
        assert_eq!(profiles.requested(), None);
        assert_eq!(profiles.request_switch(2), Ok(()));
        assert_eq!(profiles.requested(), Some(2));
        assert_eq!(profiles.request_new(), Err(ErrorCode::LimitExceeded));
        let _ = std::fs::remove_dir_all(base.parent().unwrap());
    }

    /// GIVEN a switch THEN the new core gets the given base again and the
    /// slot only when it is not 1.
    #[test]
    fn given_a_switch_then_the_new_core_gets_the_base_and_slot() {
        let profiles = Profiles::new(PathBuf::from("p"), Some(PathBuf::from("p")), 2).unwrap();
        assert_eq!(profiles.arguments(1), ["--profile", "p"]);
        assert_eq!(profiles.arguments(3), ["--profile", "p", "--account", "3"]);
        let default = Profiles::new(PathBuf::from("p"), None, 1).unwrap();
        assert!(default.arguments(1).is_empty());
        assert_eq!(default.arguments(2), ["--account", "2"]);
    }
}
