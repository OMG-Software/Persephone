//! The session owner over real SDK stores: admission, generation fencing,
//! retained ownership across a lock timeout, and unlocking.

use matrix_sdk_crypto::store::CryptoStore;
use persephone_contracts::StoreState;
use persephone_core::{
    session::{AdmissionError, MAX_LEASES, Session},
    store::{self, KEY_FILE},
};
use std::{
    fs,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};
use tempfile::TempDir;

mod common;
use common::Tagged;

const MARKER: &[u8] = b"PERSEPHONE_SYNTHETIC_SESSION_MARKER_19E4";
const LONG: Duration = Duration::from_secs(30);

fn open(dir: &Path) -> Session {
    let session = Session::open(dir.to_path_buf(), Box::new(Tagged(1)));
    assert_eq!(session.state(), StoreState::Protected);
    session
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
            store::open(dir.path(), &Tagged(1)).err(),
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
    assert!(store::open(dir.path(), &Tagged(1)).unwrap().close());
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));

    assert_eq!(session.unlock(), StoreState::Protected);
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
        store::open(dir.path(), &Tagged(1)).err(),
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
    assert_eq!(session.unlock(), StoreState::KeyMissing);
    assert!(!dir.path().join(KEY_FILE).exists(), "no replacement key");
    assert_eq!(session.admit().err(), Some(AdmissionError::Locked));
    fs::write(dir.path().join(KEY_FILE), key).unwrap();
    assert_eq!(session.unlock(), StoreState::Protected);
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
        assert_eq!(session.unlock(), StoreState::Protected, "round {round}");
    }
}
