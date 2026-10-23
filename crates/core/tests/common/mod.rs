//! Helpers shared by the core's integration tests. Each test program uses a
//! different subset.
#![allow(dead_code)]

use persephone_core::{
    credentials::{CredentialSource, Credentials, PromptError, SecretPrompt},
    passphrase::{KdfParams, Unlock},
    session::Session,
    vault::{KeyProtector, VaultError},
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use zeroize::Zeroizing;

/// The passphrase every test profile is created with, unless a script says
/// otherwise.
pub const PASSPHRASE: &str = "correct horse battery";

/// Reversible but keyed by a tag, so a blob from another "context" fails like
/// DPAPI's would. Test only; not protection.
pub struct Tagged(pub u8);

impl KeyProtector for Tagged {
    fn protect(&self, data: &[u8]) -> Result<Vec<u8>, VaultError> {
        let mut blob = vec![self.0];
        blob.extend(data.iter().map(|b| b ^ 0xA5));
        Ok(blob)
    }

    fn unprotect(&self, blob: &[u8]) -> Result<Zeroizing<Vec<u8>>, VaultError> {
        match blob.split_first() {
            Some((&tag, rest)) if tag == self.0 && !rest.is_empty() => {
                Ok(Zeroizing::new(rest.iter().map(|b| b ^ 0xA5).collect()))
            }
            _ => Err(VaultError::Failed),
        }
    }
}

/// Answers passphrase prompts from a script, then with [`PASSPHRASE`], and
/// records every prompt it was shown.
pub struct Script {
    answers: Mutex<VecDeque<Result<String, PromptError>>>,
    seen: Mutex<Vec<String>>,
    reset: Result<bool, PromptError>,
}

impl Script {
    pub fn new<'a>(answers: impl IntoIterator<Item = Result<&'a str, PromptError>>) -> Arc<Self> {
        Self::answering_reset(answers, Err(PromptError::Unsupported))
    }

    /// Also answers the reset warning with `reset`.
    pub fn answering_reset<'a>(
        answers: impl IntoIterator<Item = Result<&'a str, PromptError>>,
        reset: Result<bool, PromptError>,
    ) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(
                answers
                    .into_iter()
                    .map(|answer| answer.map(str::to_owned))
                    .collect(),
            ),
            seen: Mutex::default(),
            reset,
        })
    }

    /// The prompts shown so far, as `Debug` text.
    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

impl CredentialSource for Script {
    fn ask(&self, _: &str) -> Result<Credentials, PromptError> {
        Err(PromptError::Unsupported)
    }

    fn ask_secret(&self, prompt: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        self.seen.lock().unwrap().push(format!("{prompt:?}"));
        let answer = self.answers.lock().unwrap().pop_front();
        answer
            .unwrap_or(Ok(PASSPHRASE.to_owned()))
            .map(Zeroizing::new)
    }

    fn confirm_reset(&self) -> Result<bool, PromptError> {
        self.seen.lock().unwrap().push("ConfirmReset".into());
        self.reset
    }
}

/// Test settings: the cheapest Argon2id parameters and no backoff, so a
/// profile opens in milliseconds.
pub fn unlock_with(source: Arc<dyn CredentialSource>) -> Unlock {
    Unlock {
        source,
        kdf: KdfParams::new(64, 1, 1).unwrap(),
        backoff: Duration::ZERO,
    }
}

/// [`unlock_with`] a source that always answers [`PASSPHRASE`].
pub fn unlock() -> Unlock {
    unlock_with(Script::new([]))
}

/// A session over the profile in `dir`, opened with [`unlock`].
pub fn open_session(dir: impl Into<PathBuf>) -> Session {
    Session::open(dir.into(), Arc::new(Tagged(1)), unlock())
}
