use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use persephone_test_matrix::lifecycle_fixture::{AdmissionError, LifecycleOwner, Phase, StopError};
use tokio::{sync::Notify, time};

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::test]
async fn canceled_stop_keeps_admission_closed_and_can_be_retried() {
    let mut owner = LifecycleOwner::new();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let task_entered = entered.clone();
    let task_release = release.clone();
    owner
        .spawn(async move {
            task_entered.notify_one();
            task_release.notified().await;
            Ok(())
        })
        .unwrap();
    entered.notified().await;

    {
        let stop = owner.stop(TEST_TIMEOUT);
        tokio::pin!(stop);
        assert!(
            time::timeout(Duration::from_millis(20), &mut stop)
                .await
                .is_err()
        );
    }
    assert_eq!(owner.phase(), Phase::Draining);
    assert!(matches!(
        owner.admission().admit(),
        Err(AdmissionError::Closed)
    ));

    release.notify_one();
    assert!(
        time::timeout(TEST_TIMEOUT, owner.stop(TEST_TIMEOUT))
            .await
            .unwrap()
            .is_ok()
    );
    assert_eq!(owner.phase(), Phase::TrackedDrained);
}

#[tokio::test]
async fn deadline_fences_stale_callback_and_later_retry_drains() {
    let mut owner = LifecycleOwner::new();
    let lease = owner.admission().admit().unwrap();
    assert_eq!(lease.commit(|| 7).unwrap(), 7);

    assert_eq!(
        owner.stop(Duration::from_millis(10)).await.unwrap_err(),
        StopError::TimedOut
    );
    assert_eq!(owner.phase(), Phase::TimedOut);
    assert!(matches!(
        lease.commit(|| panic!("stale callback committed")),
        Err(AdmissionError::Closed)
    ));

    drop(lease);
    assert!(owner.stop(TEST_TIMEOUT).await.is_ok());
    assert_eq!(owner.phase(), Phase::TrackedDrained);
}

#[tokio::test]
async fn task_error_and_panic_fail_closed_after_all_jobs_finish() {
    let mut owner = LifecycleOwner::new();
    let finished = Arc::new(AtomicUsize::new(0));

    let first = finished.clone();
    owner
        .spawn(async move {
            first.fetch_add(1, Ordering::SeqCst);
            Err(())
        })
        .unwrap();

    let second = finished.clone();
    owner
        .spawn(async move {
            second.fetch_add(1, Ordering::SeqCst);
            panic!("tracked task panic");
            #[allow(unreachable_code)]
            Ok(())
        })
        .unwrap();

    assert_eq!(
        owner.stop(TEST_TIMEOUT).await.unwrap_err(),
        StopError::WorkFailed
    );
    assert_eq!(owner.phase(), Phase::Failed);
    assert_eq!(finished.load(Ordering::SeqCst), 2);
    assert!(matches!(
        owner.admission().admit(),
        Err(AdmissionError::Closed)
    ));
}

#[tokio::test]
async fn dropping_owner_closes_admission_without_claiming_drain() {
    let owner = LifecycleOwner::new();
    let admission = owner.admission();
    drop(owner);
    assert!(matches!(admission.admit(), Err(AdmissionError::Closed)));
}

#[tokio::test]
async fn admission_is_bounded_and_sdk_drain_is_never_claimed() {
    let mut owner = LifecycleOwner::new();
    let admission = owner.admission();
    let leases: Vec<_> = (0..8).map(|_| admission.admit().unwrap()).collect();
    assert!(matches!(admission.admit(), Err(AdmissionError::Saturated)));
    drop(leases);

    let tracked = owner.stop(TEST_TIMEOUT).await.unwrap();
    assert!(tracked.require_full_sdk_drain().is_err());
    assert_eq!(owner.phase(), Phase::TrackedDrained);
}

#[tokio::test]
async fn completed_jobs_still_count_toward_bounded_retained_handles() {
    let mut owner = LifecycleOwner::new();
    let (ready_tx, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut releases = Vec::new();

    for _ in 0..8 {
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        releases.push(release_tx);
        let ready = ready_tx.clone();
        owner
            .spawn(async move {
                let _ = release_rx.await;
                let _ = ready.send(());
                Ok(())
            })
            .unwrap();
    }

    for release in releases {
        let _ = release.send(());
    }
    for _ in 0..8 {
        time::timeout(TEST_TIMEOUT, ready_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(matches!(
        owner.spawn(async { Ok(()) }),
        Err(AdmissionError::Saturated)
    ));
    assert!(owner.stop(TEST_TIMEOUT).await.is_ok());
}

#[tokio::test]
async fn panicking_commit_poison_latches_failure_and_closes_admission() {
    let mut owner = LifecycleOwner::new();
    let lease = owner.admission().admit().unwrap();

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = lease.commit(|| panic!("callback panic"));
    }));
    assert!(panic.is_err());
    assert!(matches!(
        owner.admission().admit(),
        Err(AdmissionError::Closed)
    ));

    drop(lease);
    assert_eq!(
        owner.stop(TEST_TIMEOUT).await.unwrap_err(),
        StopError::WorkFailed
    );
    assert_eq!(owner.phase(), Phase::Failed);
}

#[tokio::test]
async fn failed_job_closes_admission_and_fences_other_work_before_stop() {
    let mut owner = LifecycleOwner::new();
    let other = owner.admission().admit().unwrap();
    owner.spawn(async { Err(()) }).unwrap();
    time::timeout(TEST_TIMEOUT, async {
        while owner.phase() != Phase::Failed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        owner.admission().admit(),
        Err(AdmissionError::Closed)
    ));
    assert!(matches!(
        other.commit(|| panic!("failed generation published")),
        Err(AdmissionError::Closed)
    ));
    drop(other);
    assert_eq!(
        owner.stop(TEST_TIMEOUT).await.unwrap_err(),
        StopError::WorkFailed
    );
}

#[tokio::test]
async fn stop_signals_cooperative_job_without_aborting_it() {
    let mut owner = LifecycleOwner::new();
    let admission = owner.admission();
    let finished = Arc::new(AtomicUsize::new(0));
    let completed = finished.clone();
    owner
        .spawn(async move {
            admission.wait_for_stop().await;
            assert!(admission.stop_requested());
            completed.store(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    owner.stop(TEST_TIMEOUT).await.unwrap();
    assert_eq!(finished.load(Ordering::SeqCst), 1);
}
