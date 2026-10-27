use std::{
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use matrix_sdk::{
    SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    ruma::events::room::message::OriginalSyncRoomMessageEvent,
    sliding_sync::{SlidingSyncList, SlidingSyncMode, Version},
};
use persephone_test_matrix::client;
use serde_json::json;
use tokio::{sync::Notify, task::JoinHandle, time};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_TIMEOUT: Duration = Duration::from_millis(40);
const MAX_ADMITTED_TASKS: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Running,
    Draining,
    TimedOut,
    Stopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EngineMode {
    Sliding,
    Classic,
}

struct GateState {
    phase: Phase,
    mode: EngineMode,
    active: usize,
}

struct DrainGate {
    state: Mutex<GateState>,
    changed: Notify,
}

impl DrainGate {
    fn new(mode: EngineMode) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                phase: Phase::Running,
                mode,
                active: 0,
            }),
            changed: Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().expect("drain state mutex poisoned")
    }

    fn admit(self: &Arc<Self>) -> Result<WorkLease, AdmissionError> {
        let mut state = self.lock();
        if state.phase != Phase::Running {
            return Err(AdmissionError::Closed);
        }
        if state.active >= MAX_ADMITTED_TASKS {
            return Err(AdmissionError::AtCapacity);
        }
        state.active += 1;
        Ok(WorkLease(Arc::clone(self)))
    }

    fn begin_stop(&self) {
        let mut state = self.lock();
        if state.phase == Phase::Running {
            state.phase = Phase::Draining;
        }
    }

    fn set_timed_out(&self) {
        let mut state = self.lock();
        if state.phase == Phase::Draining && state.active != 0 {
            state.phase = Phase::TimedOut;
        }
    }

    async fn wait_until_idle(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.lock().active == 0 {
                return;
            }
            changed.await;
        }
    }

    fn finish_stop(&self) -> Result<(), AdmissionError> {
        let mut state = self.lock();
        if !matches!(state.phase, Phase::Draining | Phase::TimedOut) || state.active != 0 {
            return Err(AdmissionError::StillActive);
        }
        state.phase = Phase::Stopped;
        Ok(())
    }

    fn begin_replacement(self: &Arc<Self>, mode: EngineMode) -> Result<WorkLease, AdmissionError> {
        let mut state = self.lock();
        if state.phase != Phase::Stopped || state.active != 0 {
            return Err(AdmissionError::Closed);
        }
        state.phase = Phase::Running;
        state.mode = mode;
        state.active = 1;
        Ok(WorkLease(Arc::clone(self)))
    }

    fn mode(&self) -> EngineMode {
        self.lock().mode
    }
}

struct WorkLease(Arc<DrainGate>);

impl Drop for WorkLease {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.active -= 1;
        let idle = state.active == 0;
        drop(state);
        if idle {
            self.0.changed.notify_waiters();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionError {
    Closed,
    AtCapacity,
    StillActive,
}

#[derive(Debug)]
enum StopError {
    TimedOut,
    TaskFailed,
    Failed,
    Admission,
}

struct Supervisor {
    gate: Arc<DrainGate>,
    task: Option<JoinHandle<Result<(), String>>>,
}

impl Supervisor {
    fn new<F>(gate: Arc<DrainGate>, future: F, engine_lease: WorkLease) -> Self
    where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        Self {
            gate,
            task: Some(tokio::spawn(async move {
                let _engine_lease = engine_lease;
                future.await
            })),
        }
    }

    fn try_start_replacement<F>(
        &mut self,
        mode: EngineMode,
        future: F,
    ) -> Result<(), AdmissionError>
    where
        F: Future<Output = Result<(), String>> + Send + 'static,
    {
        let lease = self.gate.begin_replacement(mode)?;
        self.task = Some(tokio::spawn(async move {
            let _engine_lease = lease;
            future.await
        }));
        Ok(())
    }

    async fn request_stop(&mut self, timeout: Duration) -> Result<(), StopError> {
        self.gate.begin_stop();
        if time::timeout(timeout, self.gate.wait_until_idle())
            .await
            .is_err()
        {
            self.gate.set_timed_out();
            return Err(StopError::TimedOut);
        }

        let task = self.task.take().ok_or(StopError::Admission)?;
        match task.await.map_err(|_| StopError::TaskFailed)? {
            Ok(()) => self.gate.finish_stop().map_err(|_| StopError::Admission),
            Err(_) => Err(StopError::Failed),
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.gate.begin_stop();
    }
}

#[tokio::test]
async fn given_paused_sdk_handler_when_stop_times_out_then_replacement_waits_for_cooperative_drain()
{
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/_matrix/client/versions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "versions": ["v1.19"],
            "unstable_features": {"org.matrix.simplified_msc3575": true}
        })))
        .mount(&server)
        .await;

    let room_id = "!p0:synthetic.test";
    Mock::given(method("POST"))
        .and(path(
            "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pos": "drain-position",
            "lists": {"rooms": {"count": 1}},
            "rooms": {room_id: {
                "initial": true,
                "name": "synthetic",
                "joined_count": 1,
                "invited_count": 0,
                "required_state": [
                    {"type": "m.room.create", "state_key": "", "sender": "@p0_alice:synthetic.test", "event_id": "$create:synthetic.test", "origin_server_ts": 1, "content": {"room_version": "11"}},
                    {"type": "m.room.member", "state_key": "@p0_alice:synthetic.test", "sender": "@p0_alice:synthetic.test", "event_id": "$join:synthetic.test", "origin_server_ts": 2, "content": {"membership": "join"}}
                ],
                "timeline": [{"type": "m.room.message", "sender": "@p0_alice:synthetic.test", "event_id": "$message:synthetic.test", "origin_server_ts": 3, "content": {"msgtype": "m.text", "body": "synthetic"}}],
                "num_live": 1,
                "prev_batch": "history-position",
                "notification_count": 0,
                "highlight_count": 0
            }},
            "extensions": {}
        })))
        .mount(&server)
        .await;

    let client = client(&server.uri()).await.unwrap();
    client
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: "@p0_alice:synthetic.test".parse().unwrap(),
                device_id: "P0DEVICE".into(),
            },
            tokens: SessionTokens {
                access_token: "synthetic-drain-fixture-token".into(),
                refresh_token: None,
            },
        })
        .await
        .unwrap();

    let gate = DrainGate::new(EngineMode::Sliding);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let callback = (
        gate.clone(),
        entered.clone(),
        release.clone(),
        completed.clone(),
    );
    client.add_event_handler(move |_event: OriginalSyncRoomMessageEvent| {
        let (gate, entered, release, completed) = callback.clone();
        async move {
            let Ok(_work_lease) = gate.admit() else {
                return;
            };
            entered.notify_one();
            release.notified().await;
            completed.notify_one();
        }
    });

    let sliding = client
        .sliding_sync("p0-drain")
        .unwrap()
        .version(Version::Native)
        .add_list(
            SlidingSyncList::builder("rooms")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=9)),
        )
        .build()
        .await
        .unwrap();
    let engine_lease = gate.admit().unwrap();
    let sync = async move {
        sliding
            .sync_once()
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    };
    let mut supervisor = Supervisor::new(gate.clone(), sync, engine_lease);

    time::timeout(TEST_TIMEOUT, entered.notified())
        .await
        .expect("SDK event handler should start");

    assert!(matches!(
        supervisor.request_stop(STOP_TIMEOUT).await,
        Err(StopError::TimedOut)
    ));
    assert_eq!(gate.mode(), EngineMode::Sliding);
    assert_eq!(gate.admit().err(), Some(AdmissionError::Closed));
    assert_eq!(
        supervisor.try_start_replacement(EngineMode::Classic, async { Ok(()) }),
        Err(AdmissionError::Closed)
    );

    release.notify_one();
    time::timeout(TEST_TIMEOUT, completed.notified())
        .await
        .expect("paused SDK handler should complete after release");
    supervisor
        .request_stop(TEST_TIMEOUT)
        .await
        .expect("drain should complete only after admitted work and sync future finish");

    let (replacement_started_tx, replacement_started_rx) = tokio::sync::oneshot::channel();
    supervisor
        .try_start_replacement(EngineMode::Classic, async move {
            let _ = replacement_started_tx.send(());
            Ok(())
        })
        .unwrap();
    time::timeout(TEST_TIMEOUT, replacement_started_rx)
        .await
        .expect("classic replacement should be admitted after drain")
        .expect("replacement task should signal start");
    supervisor
        .request_stop(TEST_TIMEOUT)
        .await
        .expect("replacement probe task should finish cleanly");
    assert_eq!(gate.mode(), EngineMode::Classic);
}
