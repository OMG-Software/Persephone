//! The session owner over real SDK stores: admission, generation fencing,
//! retained ownership across a lock timeout, and unlocking.

use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::SignInError;
use persephone_contracts::StoreState;
use persephone_core::{
    credentials::{CredentialSource, Credentials, PromptError, SecretPrompt},
    session::{AdmissionError, MAX_LEASES, Session, SessionListener},
    store::{self, KEY_FILE},
};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tempfile::TempDir;

mod common;
use common::{PASSPHRASE, Script, Tagged, unlock, unlock_with};
use std::sync::{Arc, Mutex};
use zeroize::Zeroizing;

const MARKER: &[u8] = b"PERSEPHONE_SYNTHETIC_SESSION_MARKER_19E4";
const LONG: Duration = Duration::from_secs(30);

fn open(dir: &Path) -> Session {
    let session = common::open_session(dir);
    assert_eq!(session.state(), StoreState::Protected);
    session
}

/// The state once an unlock or reset worker has finished.
fn settled(session: &Session) -> StoreState {
    let deadline = Instant::now() + LONG;
    while session.state() == StoreState::Unlocking && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    session.state()
}

/// A lease held on another thread keeps the lock waiting: new work and the
/// old lease's results are refused at once, and the stores close only after
/// the lease ends.
#[test]
fn given_admitted_work_when_locking_then_it_finishes_before_the_stores_close() {
    let dir = TempDir::new().unwrap();
    let session = open(dir.path());
    let announced = AtomicUsize::new(0);
    let (started, lease_started) = mpsc::channel();
    let (committed, commit_result) = mpsc::channel();

    std::thread::scope(|scope| {
        // Created inside the scope: if an assertion below fails, unwinding
        // drops it, the lease thread's `recv` fails, and the test ends instead
        // of every thread waiting on another.
        let (finish, lease_finish) = mpsc::channel::<()>();
        let lease = session.admit().unwrap();
        scope.spawn(move || {
            lease
                .stores()
                .block_on(
                    lease
                        .stores()
                        .crypto()
                        .set_custom_value("marker", MARKER.to_vec()),
                )
                .unwrap();
            started.send(()).unwrap();
            lease_finish.recv().unwrap();
            // The lock began while this work ran: its result must not land.
            committed.send(lease.commit(|| ())).unwrap();
        });
        lease_started.recv().unwrap();

        let locker = scope.spawn(|| {
            session.lock(LONG, || {
                announced.fetch_add(1, Ordering::SeqCst);
            })
        });
        // The state turns `locking` just before the announcement runs; the
        // announcement must come while the lease still holds the lock waiting.
        while session.state() != StoreState::Locking || announced.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        assert_eq!(
            announced.load(Ordering::SeqCst),
            1,
            "announced once, before waiting"
        );
        assert_eq!(session.admit().err(), Some(AdmissionError::Locked));
        // Still open: another owner cannot take the profile.
        assert_eq!(
            store::open(dir.path(), &Tagged(1), &unlock()).err(),
            Some(StoreState::ProfileInUse)
        );

        finish.send(()).unwrap();
        let (state, drain) = locker.join().unwrap();
        assert_eq!(state, StoreState::Locked);
        assert!(
            drain.expect("drained").require_full_sdk_drain().is_err(),
            "leased work is never a full SDK drain"
        );
    });
    assert_eq!(commit_result.recv().unwrap(), Err(AdmissionError::Locked));
    // The stores are closed and the profile released: another owner can open
    // it (and closes it again). SQLite's WAL files are not proof either way:
    // when two pool connections close at once, neither may remove them.
    assert!(
        store::open(dir.path(), &Tagged(1), &unlock())
            .unwrap()
            .close()
    );
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));

    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::Protected);
    let lease = session.admit().unwrap();
    let marker = lease
        .stores()
        .block_on(lease.stores().crypto().get_custom_value("marker"))
        .unwrap();
    assert_eq!(marker.as_deref(), Some(MARKER));
    assert_eq!(lease.commit(|| 7), Ok(7), "a new generation commits");
}

/// A timeout keeps the session locking with the stores open and owned; a
/// later lock resumes the wait without announcing again.
#[test]
fn given_a_lock_timeout_then_ownership_is_kept_and_a_retry_completes_it() {
    let dir = TempDir::new().unwrap();
    let session = open(dir.path());
    let announced = AtomicUsize::new(0);
    let announce = || {
        announced.fetch_add(1, Ordering::SeqCst);
    };
    let lease = session.admit().unwrap();

    let (state, drain) = session.lock(Duration::from_millis(50), announce);
    assert_eq!(state, StoreState::Locking);
    assert!(drain.is_none());
    assert_eq!(
        store::open(dir.path(), &Tagged(1), &unlock()).err(),
        Some(StoreState::ProfileInUse),
        "stores still held after the timeout"
    );
    assert_eq!(session.unlock(), StoreState::Locking, "no unlock mid-lock");
    assert_eq!(lease.commit(|| ()), Err(AdmissionError::Locked));

    drop(lease);
    let (state, drain) = session.lock(LONG, announce);
    assert_eq!(state, StoreState::Locked);
    assert!(drain.is_some());
    assert_eq!(announced.load(Ordering::SeqCst), 1, "announced once");

    let (state, _) = session.lock(LONG, announce);
    assert_eq!(state, StoreState::Locked, "locking again is a no-op");
    assert_eq!(announced.load(Ordering::SeqCst), 1);
}

#[test]
fn given_a_locked_session_whose_key_is_gone_then_unlock_stays_locked_until_it_returns() {
    let dir = TempDir::new().unwrap();
    let session = open(dir.path());
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);
    let key = fs::read(dir.path().join(KEY_FILE)).unwrap();
    fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::KeyMissing);
    assert!(!dir.path().join(KEY_FILE).exists(), "no replacement key");
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));
    fs::write(dir.path().join(KEY_FILE), key).unwrap();
    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::Protected);
}

#[test]
fn given_more_leases_than_the_limit_then_admission_is_saturated() {
    let dir = TempDir::new().unwrap();
    let session = open(dir.path());
    let leases: Vec<_> = (0..MAX_LEASES).map(|_| session.admit().unwrap()).collect();
    assert_eq!(session.admit().err(), Some(AdmissionError::Saturated));
    drop(leases);
    assert!(session.admit().is_ok());
}

#[test]
fn given_no_profile_then_nothing_is_admitted_and_lock_or_unlock_change_nothing() {
    let session = Session::not_configured();
    assert!(!session.is_configured());
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));
    assert_eq!(
        session.lock(LONG, || panic!("nothing to announce")).0,
        StoreState::NotConfigured
    );
    assert_eq!(session.unlock(), StoreState::NotConfigured);
}

/// GIVEN no usable default profile directory THEN the session is configured,
/// so the views stay locked instead of synthetic, and nothing opens it.
#[test]
fn given_no_default_profile_directory_then_the_session_stays_unavailable() {
    let session = Session::unavailable();
    assert!(session.is_configured());
    assert_eq!(session.state(), StoreState::StoreUnavailable);
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));
    assert_eq!(session.unlock(), StoreState::StoreUnavailable);
    assert_eq!(session.reset_profile(), StoreState::StoreUnavailable);
    assert_eq!(
        session.lock(LONG, || panic!("nothing to announce")).0,
        StoreState::StoreUnavailable
    );
}

/// GIVEN a profile in directories that do not exist yet THEN each directory
/// created for it is private to the user, and an existing parent keeps its
/// mode.
#[cfg(unix)]
#[test]
fn given_a_new_profile_path_then_the_created_directories_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = TempDir::new().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
    let dir = root.path().join("vendor").join("profile");
    let _session = open(&dir);
    let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&root.path().join("vendor")), 0o700);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(root.path()), 0o755);
}

/// A lease that ends while a lock waits must never make the lock fail. The
/// lease used to wake the locker before releasing its reference to the
/// stores, so the locker could find them still shared and report
/// `lock_failed`. Repeats the race; it is timing-dependent, not a proof.
#[test]
fn given_a_lease_ending_during_a_lock_then_the_lock_completes() {
    let dir = TempDir::new().unwrap();
    let session = open(dir.path());
    for round in 0..200 {
        let lease = session.admit().unwrap();
        std::thread::scope(|scope| {
            let locker = scope.spawn(|| session.lock(LONG, || {}));
            while session.state() != StoreState::Locking {
                std::thread::yield_now();
            }
            drop(lease);
            let (state, drain) = locker.join().unwrap();
            assert_eq!(state, StoreState::Locked, "round {round}");
            assert!(drain.is_some());
        });
        assert_eq!(session.unlock(), StoreState::Unlocking, "round {round}");
        assert_eq!(settled(&session), StoreState::Protected, "round {round}");
    }
}

fn open_with(dir: &Path, source: Arc<dyn CredentialSource>) -> Session {
    let session = Session::open(dir.to_path_buf(), Arc::new(Tagged(1)), unlock_with(source));
    assert_eq!(session.state(), StoreState::Protected);
    session
}

fn write_marker(session: &Session) {
    let lease = session.admit().unwrap();
    lease
        .stores()
        .block_on(
            lease
                .stores()
                .crypto()
                .set_custom_value("marker", MARKER.to_vec()),
        )
        .unwrap();
}

fn read_marker(session: &Session) -> Option<Vec<u8>> {
    let lease = session.admit().unwrap();
    lease
        .stores()
        .block_on(lease.stores().crypto().get_custom_value("marker"))
        .unwrap()
}

/// GIVEN a locked session WHEN the passphrase is wrong five times at Unlock
/// THEN the session is `secret_rejected` with nothing admitted, the waits
/// were asked for with `retry`, and a later Unlock with the right passphrase
/// opens the same data.
#[test]
fn given_wrong_passphrases_at_unlock_then_the_session_is_rejected_until_a_right_one() {
    let dir = TempDir::new().unwrap();
    // Two answers create the profile; then one empty and four wrong entries.
    let script = Script::new(
        [Ok(PASSPHRASE), Ok(PASSPHRASE), Err(PromptError::Invalid)]
            .into_iter()
            .chain([Ok("not the passphrase"); 4]),
    );
    let session = open_with(dir.path(), script.clone());
    write_marker(&session);
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);

    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::SecretRejected);
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));
    let seen = script.seen();
    assert_eq!(seen.len(), 7, "{seen:?}");
    assert_eq!(seen[2], "Passphrase { retry: false }");
    assert!(seen[3..].iter().all(|p| p == "Passphrase { retry: true }"));

    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::Protected);
    assert_eq!(read_marker(&session).as_deref(), Some(MARKER));
}

/// GIVEN a locked session WHEN the prompt is cancelled, or cannot be shown
/// THEN the session stays locked and nothing is written; Unlock can retry.
#[test]
fn given_a_cancelled_or_failed_prompt_at_unlock_then_the_session_stays_locked() {
    let dir = TempDir::new().unwrap();
    let script = Script::new([
        Ok(PASSPHRASE),
        Ok(PASSPHRASE),
        Err(PromptError::Cancelled),
        Err(PromptError::Failed),
    ]);
    let session = open_with(dir.path(), script);
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);
    let key = fs::read(dir.path().join(KEY_FILE)).unwrap();
    for _ in 0..2 {
        assert_eq!(session.unlock(), StoreState::Unlocking);
        assert_eq!(settled(&session), StoreState::Locked);
        assert_eq!(fs::read(dir.path().join(KEY_FILE)).unwrap(), key);
    }
    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::Protected);
}

/// Answers the two prompts that create a profile, then holds every later
/// prompt until released.
struct Gate {
    asked: Mutex<usize>,
    open: Mutex<bool>,
    wake: std::sync::Condvar,
}

impl CredentialSource for Gate {
    fn ask(&self, _: &str) -> Result<Credentials, PromptError> {
        Err(PromptError::Unsupported)
    }

    fn ask_secret(&self, _: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        let first = {
            let mut asked = self.asked.lock().unwrap();
            *asked += 1;
            *asked <= 2
        };
        if !first {
            let open = self.open.lock().unwrap();
            drop(self.wake.wait_while(open, |open| !*open).unwrap());
        }
        Ok(Zeroizing::new(PASSPHRASE.into()))
    }
}

/// GIVEN an Unlock whose prompt is still open WHEN lock, unlock or reset is
/// asked THEN each returns `unlocking` at once without changing anything;
/// once the prompt is answered the session opens.
#[test]
fn given_an_open_unlock_prompt_then_other_transitions_wait_without_blocking() {
    let dir = TempDir::new().unwrap();
    let gate = Arc::new(Gate {
        asked: Mutex::new(0),
        open: Mutex::new(false),
        wake: std::sync::Condvar::new(),
    });
    let session = open_with(dir.path(), gate.clone());
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);
    assert_eq!(session.unlock(), StoreState::Unlocking);
    while *gate.asked.lock().unwrap() < 3 {
        std::thread::yield_now();
    }
    let started = Instant::now();
    assert_eq!(
        session.lock(LONG, || panic!("nothing to announce")).0,
        StoreState::Unlocking
    );
    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(session.reset_profile(), StoreState::Unlocking);
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));
    assert!(started.elapsed() < Duration::from_secs(5), "nothing waited");

    *gate.open.lock().unwrap() = true;
    gate.wake.notify_all();
    assert_eq!(settled(&session), StoreState::Protected);
    assert_eq!(*gate.asked.lock().unwrap(), 3, "one prompt for one unlock");
}

/// GIVEN a locked profile with data WHEN a reset is declined in the core's
/// warning, or the warning cannot be shown THEN the session is back as it
/// was and the data is kept.
#[test]
fn given_a_declined_reset_then_the_profile_is_kept() {
    for answer in [Ok(false), Err(PromptError::Failed)] {
        let dir = TempDir::new().unwrap();
        let script = Script::answering_reset([], answer);
        let session = open_with(dir.path(), script.clone());
        write_marker(&session);
        assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);

        assert_eq!(session.reset_profile(), StoreState::Unlocking);
        assert_eq!(settled(&session), StoreState::Locked, "{answer:?}");
        assert_eq!(
            script.seen().last().map(String::as_str),
            Some("ConfirmReset")
        );
        assert!(dir.path().join(KEY_FILE).exists());

        assert_eq!(session.unlock(), StoreState::Unlocking);
        assert_eq!(settled(&session), StoreState::Protected);
        assert_eq!(read_marker(&session).as_deref(), Some(MARKER));
    }
}

/// GIVEN a profile rejected after a forgotten passphrase WHEN a reset is
/// confirmed THEN the old data is gone, a new passphrase makes a new empty
/// profile, and the account says the old session was not revoked.
#[test]
fn given_a_confirmed_reset_then_a_new_empty_profile_opens_with_a_new_passphrase() {
    let dir = TempDir::new().unwrap();
    let script = Script::answering_reset(
        [Ok(PASSPHRASE), Ok(PASSPHRASE)]
            .into_iter()
            .chain([Ok("forgotten it"); 5])
            .chain([Ok("a new passphrase"), Ok("a new passphrase")]),
        Ok(true),
    );
    let session = open_with(dir.path(), script.clone());
    write_marker(&session);
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);
    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::SecretRejected);

    assert_eq!(session.reset_profile(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::Protected);
    assert_eq!(read_marker(&session), None, "old data gone");
    assert_eq!(
        session.account_status().error,
        Some(SignInError::SessionNotRevoked)
    );
    assert_eq!(
        script.seen()[7..],
        [
            "ConfirmReset",
            "NewPassphrase(Choose)",
            "NewPassphrase(Confirm)"
        ]
    );
    assert!(!dir.path().join(store::SIGNED_OUT_FILE).exists());

    // The new passphrase opens it; the old one does not.
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);
    let reopened = Script::new([Ok("a new passphrase")]);
    let other = store::open(dir.path(), &Tagged(1), &unlock_with(reopened)).unwrap();
    assert!(other.close());
    let old = Script::new([Ok(PASSPHRASE); 5]);
    assert_eq!(
        store::open(dir.path(), &Tagged(1), &unlock_with(old)).err(),
        Some(StoreState::SecretRejected)
    );
}

/// GIVEN an open profile, or no profile WHEN a reset is asked THEN nothing
/// is asked and nothing changes: only a closed profile can be reset.
#[test]
fn given_an_open_or_missing_profile_then_reset_is_refused() {
    let dir = TempDir::new().unwrap();
    let script = Script::answering_reset([], Ok(true));
    let session = open_with(dir.path(), script.clone());
    write_marker(&session);
    let asked = script.seen().len();
    assert_eq!(session.reset_profile(), StoreState::Protected);
    assert_eq!(script.seen().len(), asked, "no warning shown");
    assert_eq!(read_marker(&session).as_deref(), Some(MARKER));

    assert_eq!(
        Session::not_configured().reset_profile(),
        StoreState::NotConfigured
    );
}

/// Answers the two prompts that create a profile, then panics once.
struct PanicsOnce {
    asked: Mutex<usize>,
}

impl CredentialSource for PanicsOnce {
    fn ask(&self, _: &str) -> Result<Credentials, PromptError> {
        Err(PromptError::Unsupported)
    }

    fn ask_secret(&self, _: SecretPrompt<'_>) -> Result<Zeroizing<String>, PromptError> {
        let asked = {
            let mut asked = self.asked.lock().unwrap();
            *asked += 1;
            *asked
        };
        if asked == 3 {
            panic!("synthetic prompt failure");
        }
        Ok(Zeroizing::new(PASSPHRASE.into()))
    }
}

/// GIVEN an Unlock whose prompt panics WHEN the worker unwinds THEN the
/// session ends `store_unavailable`, not `unlocking`, the profile is
/// released, and a later Unlock opens it.
#[test]
fn given_a_panicking_prompt_at_unlock_then_the_session_does_not_stay_unlocking() {
    let dir = TempDir::new().unwrap();
    let session = open_with(
        dir.path(),
        Arc::new(PanicsOnce {
            asked: Mutex::new(0),
        }),
    );
    write_marker(&session);
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);

    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::StoreUnavailable);
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));

    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::Protected);
    assert_eq!(read_marker(&session).as_deref(), Some(MARKER));
}

/// Panics on every session change.
struct PanickingListener;

impl SessionListener for PanickingListener {
    fn rooms_changed(&self, _: &[String]) {}

    fn session_changed(&self) {
        panic!("synthetic listener failure");
    }
}

/// GIVEN a profile with an unconfirmed sign-out WHEN it is unlocked and the
/// listener panics on the announcement THEN the new state and stores are
/// already published together: the session is open with the sign-out
/// reported, not left `unlocking`.
#[test]
fn given_a_panicking_listener_at_unlock_then_the_state_is_published_first() {
    let dir = TempDir::new().unwrap();
    let session = open(dir.path());
    assert_eq!(session.lock(LONG, || {}).0, StoreState::Locked);
    store::begin_sign_out(dir.path()).unwrap();
    session.set_listener(Arc::new(PanickingListener));

    assert_eq!(session.unlock(), StoreState::Unlocking);
    assert_eq!(settled(&session), StoreState::Protected);
    assert!(session.admit().is_ok());
    assert_eq!(
        session.account_status().error,
        Some(SignInError::SessionNotRevoked)
    );
}
