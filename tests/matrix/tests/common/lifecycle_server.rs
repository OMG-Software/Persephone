//! Small, local HTTP fixture for SDK lifecycle tests.
//!
//! It deliberately serves only the endpoints needed by these tests. In
//! particular, it is not a proxy and never logs request contents.

use std::{io, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

const MAX_HEADER_BYTES: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const HELD_PATH: &str = "/_matrix/client/v3/room_keys/version";
const VERSIONS_PATH: &str = "/_matrix/client/versions";

pub struct LifecycleServer {
    origin: String,
    pub entered: Arc<Notify>,
    pub release: Arc<Notify>,
    pub completed: Arc<Notify>,
    stop: watch::Sender<bool>,
    accept_task: Option<JoinHandle<()>>,
}

impl LifecycleServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind lifecycle fixture to loopback");
        let address = listener.local_addr().expect("read fixture address");
        let origin = format!("http://{address}");

        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(Notify::new());
        let (stop, stop_rx) = watch::channel(false);

        let accept_task = tokio::spawn(accept_connections(
            listener,
            stop_rx,
            entered.clone(),
            release.clone(),
            completed.clone(),
        ));

        Self {
            origin,
            entered,
            release,
            completed,
            stop,
            accept_task: Some(accept_task),
        }
    }

    pub fn origin(&self) -> String {
        self.origin.clone()
    }

    /// Stop accepting requests and wait for the listener and all connections.
    /// A caller can wrap this in its own timeout when testing shutdown bounds.
    pub async fn close(mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.accept_task.take() {
            task.await.expect("lifecycle HTTP accept task panicked");
        }
    }
}

impl Drop for LifecycleServer {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

async fn accept_connections(
    listener: TcpListener,
    mut stop: watch::Receiver<bool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<Notify>,
) {
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { break };
                connections.spawn(handle_connection(
                    stream,
                    stop.clone(),
                    entered.clone(),
                    release.clone(),
                    completed.clone(),
                ));
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => { result.expect("lifecycle HTTP handler panicked"); }
        }
    }

    // Aborting the connection tasks also releases requests waiting on the test
    // barrier, so dropping the fixture cannot leave a gated task behind.
    connections.abort_all();
    while let Some(result) = connections.join_next().await {
        if let Err(error) = result {
            assert!(error.is_cancelled(), "lifecycle HTTP handler panicked");
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    mut stop: watch::Receiver<bool>,
    entered: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<Notify>,
) {
    if *stop.borrow() {
        return;
    }

    let request = tokio::select! {
        changed = stop.changed() => {
            let _ = changed;
            return;
        }
        request = timeout(REQUEST_TIMEOUT, read_request_line(&mut stream)) => {
            match request {
                Ok(Ok(request)) => request,
                _ => return,
            }
        }
    };

    let (status, body, held): (&str, &[u8], bool) = match request {
        Some((method, path)) if method == "GET" && path == HELD_PATH => {
            entered.notify_one();
            tokio::select! {
                changed = stop.changed() => {
                    let _ = changed;
                    return;
                }
                _ = release.notified() => {}
            }
            (
                "503 Service Unavailable",
                br#"{"errcode":"M_UNKNOWN","error":"fixture barrier released"}"#,
                true,
            )
        }
        Some((method, path)) if method == "GET" && path == VERSIONS_PATH => {
            ("200 OK", br#"{"versions":["v1.15"]}"#, false)
        }
        _ => (
            "404 Not Found",
            br#"{"errcode":"M_NOT_FOUND","error":"fixture endpoint not found"}"#,
            false,
        ),
    };

    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    if stream.write_all(response.as_bytes()).await.is_err()
        || stream.write_all(body).await.is_err()
        || stream.flush().await.is_err()
    {
        return;
    }

    if held {
        completed.notify_one();
    }
}

async fn read_request_line(stream: &mut TcpStream) -> io::Result<Option<(String, String)>> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];

    loop {
        if bytes.len() >= MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }

        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk[..read]);

        if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            if header_end + 4 > MAX_HEADER_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "request headers too large",
                ));
            }

            let line_end = bytes
                .windows(2)
                .position(|window| window == b"\r\n")
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing request line")
                })?;
            let request_line = std::str::from_utf8(&bytes[..line_end])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid request line"))?;
            let mut parts = request_line.split_whitespace();
            let Some(method) = parts.next() else {
                return Ok(None);
            };
            let Some(path) = parts.next() else {
                return Ok(None);
            };
            return Ok(Some((method.to_owned(), path.to_owned())));
        }
    }
}
