//! Daemon mode — persistent devp2p peer pool with IPC job control.
//!
//! # Architecture
//! The daemon holds the devp2p peer connection alive across CLI invocations.
//! CLI commands communicate via a Unix domain socket using newline-delimited JSON.
//!
//! # Protocol
//! - Client sends one [`DaemonRequest`] per connection.
//! - Server sends one or more [`DaemonResponse`] messages back.
//! - For streaming responses (Logs), the server keeps sending until the client disconnects.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{debug, warn};
use tracing_subscriber::Layer;

// ── Path helpers ──────────────────────────────────────────────────────────────

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon.sock")
}

pub fn pid_path(data_dir: &Path) -> PathBuf {
    data_dir.join("daemon.pid")
}

// ── Protocol messages ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonRequest {
    /// Submit a new sync job to the daemon.
    SyncJob {
        config_path: String,
        blocks_override: Option<String>,
    },
    /// Request the daemon's current status.
    Status,
    /// Stream log output. `lines` = number of backlog lines to replay first.
    Logs { lines: usize },
    /// Ask the daemon to shut down gracefully.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DaemonResponse {
    /// Sent immediately when a SyncJob is accepted (before any work begins).
    JobAccepted {
        job_id: u64,
        queue_position: usize,
    },
    /// Snapshot of an already-running job, sent to late-connecting clients.
    JobStateSnapshot {
        job_id: u64,
        stage: u8,
        pos: u64,
        total: u64,
        msg: String,
    },
    /// Progress update for the active job.
    Progress {
        job_id: u64,
        stage: u8,
        pos: u64,
        total: u64,
        msg: String,
    },
    /// Daemon-level status snapshot.
    StatusSnapshot {
        peers: u32,
        uptime_secs: u64,
        active_job: Option<JobSnapshot>,
        queue_len: usize,
    },
    /// A single captured log line (streamed for `Logs` requests).
    LogLine {
        ts: String,
        level: String,
        msg: String,
    },
    /// Emitted when a sync job finishes successfully.
    JobComplete {
        job_id: u64,
        events_found: u64,
    },
    /// Confirmation that the daemon has finished shutting down.
    ShutdownComplete,
    /// An error occurred (daemon continues running).
    Error { msg: String },
}

/// Minimal snapshot of the active job's progress, embedded in [`DaemonResponse::StatusSnapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub job_id: u64,
    pub stage: u8,
    pub pos: u64,
    pub total: u64,
}

// ── Captured log line (internal) ──────────────────────────────────────────────

/// A single formatted log record. Stored in the ring buffer and broadcast to subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub ts: String,
    pub level: String,
    pub msg: String,
}

impl From<LogLine> for DaemonResponse {
    fn from(l: LogLine) -> Self {
        DaemonResponse::LogLine { ts: l.ts, level: l.level, msg: l.msg }
    }
}

// ── Job queue entry ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SyncJobRequest {
    pub job_id: u64,
    pub config_path: String,
    pub blocks_override: Option<String>,
}

// ── Active job state (shared between accept loop and job runner) ──────────────

#[derive(Debug, Default)]
pub struct ActiveJobState {
    pub job_id: AtomicU64,
    pub stage: AtomicU32,
    pub pos: AtomicU64,
    pub total: AtomicU64,
    pub active: std::sync::atomic::AtomicBool,
}

// ── DaemonServer ─────────────────────────────────────────────────────────────

const RING_BUF_CAP: usize = 10_000;

pub struct DaemonServer {
    listener: UnixListener,
    job_tx: mpsc::Sender<SyncJobRequest>,
    event_tx: broadcast::Sender<DaemonResponse>,
    ring_buf: Arc<RwLock<VecDeque<LogLine>>>,
    token: tokio_util::sync::CancellationToken,
    peer_count: Arc<AtomicU32>,
    job_counter: Arc<AtomicU64>,
    job_queue_len: Arc<AtomicU64>,
    active_job: Arc<ActiveJobState>,
    started_at: Instant,
}

impl DaemonServer {
    pub fn new(
        listener: UnixListener,
        job_tx: mpsc::Sender<SyncJobRequest>,
        event_tx: broadcast::Sender<DaemonResponse>,
        ring_buf: Arc<RwLock<VecDeque<LogLine>>>,
        token: tokio_util::sync::CancellationToken,
        peer_count: Arc<AtomicU32>,
        job_counter: Arc<AtomicU64>,
        job_queue_len: Arc<AtomicU64>,
        active_job: Arc<ActiveJobState>,
    ) -> Self {
        Self {
            listener,
            job_tx,
            event_tx,
            ring_buf,
            token,
            peer_count,
            job_counter,
            job_queue_len,
            active_job,
            started_at: Instant::now(),
        }
    }

    pub async fn accept_loop(self) {
        let tracker = tokio_util::task::TaskTracker::new();
        loop {
            tokio::select! {
                accept = self.listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let ctx = ClientCtx {
                                job_tx: self.job_tx.clone(),
                                event_tx: self.event_tx.clone(),
                                ring_buf: Arc::clone(&self.ring_buf),
                                token: self.token.clone(),
                                peer_count: Arc::clone(&self.peer_count),
                                job_counter: Arc::clone(&self.job_counter),
                                job_queue_len: Arc::clone(&self.job_queue_len),
                                active_job: Arc::clone(&self.active_job),
                                started_at: self.started_at,
                            };
                            tracker.spawn(handle_client(stream, ctx));
                        }
                        Err(e) => {
                            warn!("daemon accept error: {e}");
                        }
                    }
                }
                _ = self.token.cancelled() => break,
            }
        }
        tracker.close();
        tracker.wait().await;
    }
}

struct ClientCtx {
    job_tx: mpsc::Sender<SyncJobRequest>,
    event_tx: broadcast::Sender<DaemonResponse>,
    ring_buf: Arc<RwLock<VecDeque<LogLine>>>,
    token: tokio_util::sync::CancellationToken,
    peer_count: Arc<AtomicU32>,
    job_counter: Arc<AtomicU64>,
    job_queue_len: Arc<AtomicU64>,
    active_job: Arc<ActiveJobState>,
    started_at: Instant,
}

async fn handle_client(stream: UnixStream, ctx: ClientCtx) {
    use tokio_stream::StreamExt;

    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(65_536));

    let line = tokio::select! {
        line = framed.next() => line,
        _ = ctx.token.cancelled() => return,
    };

    let line = match line {
        Some(Ok(l)) => l,
        Some(Err(e)) => {
            debug!("daemon client framing error: {e}");
            return;
        }
        None => return,
    };

    let request: DaemonRequest = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => {
            debug!("daemon: malformed request from client: {e}");
            return;
        }
    };

    match request {
        DaemonRequest::SyncJob { config_path, blocks_override } => {
            let job_id = ctx.job_counter.fetch_add(1, Ordering::Relaxed) + 1;
            let queue_len = ctx.job_queue_len.fetch_add(1, Ordering::Relaxed) as usize + 1;

            let job = SyncJobRequest { job_id, config_path, blocks_override };
            if ctx.job_tx.send(job).await.is_err() {
                let _ = send_response(
                    &mut framed,
                    &DaemonResponse::Error { msg: "job queue closed".into() },
                )
                .await;
                return;
            }

            let _ = send_response(
                &mut framed,
                &DaemonResponse::JobAccepted { job_id, queue_position: queue_len },
            )
            .await;

            // If a job is currently active, send a snapshot so the client can init progress bars.
            if ctx.active_job.active.load(Ordering::Relaxed) {
                let snapshot = DaemonResponse::JobStateSnapshot {
                    job_id: ctx.active_job.job_id.load(Ordering::Relaxed),
                    stage: ctx.active_job.stage.load(Ordering::Relaxed) as u8,
                    pos: ctx.active_job.pos.load(Ordering::Relaxed),
                    total: ctx.active_job.total.load(Ordering::Relaxed),
                    msg: String::new(),
                };
                let _ = send_response(&mut framed, &snapshot).await;
            }

            // Stream progress events until the job completes or client disconnects.
            let mut rx = ctx.event_tx.subscribe();
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Ok(resp @ DaemonResponse::Progress { .. })
                            | Ok(resp @ DaemonResponse::JobComplete { .. })
                            | Ok(resp @ DaemonResponse::Error { .. }) => {
                                let done = matches!(resp, DaemonResponse::JobComplete { .. });
                                if send_response(&mut framed, &resp).await.is_err() {
                                    break;
                                }
                                if done {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                debug!("daemon: client lagged, dropped {n} events");
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = ctx.token.cancelled() => break,
                }
            }
        }

        DaemonRequest::Status => {
            let uptime_secs = ctx.started_at.elapsed().as_secs();
            let peers = ctx.peer_count.load(Ordering::Relaxed);
            let queue_len = ctx.job_queue_len.load(Ordering::Relaxed) as usize;
            let active_job = if ctx.active_job.active.load(Ordering::Relaxed) {
                Some(JobSnapshot {
                    job_id: ctx.active_job.job_id.load(Ordering::Relaxed),
                    stage: ctx.active_job.stage.load(Ordering::Relaxed) as u8,
                    pos: ctx.active_job.pos.load(Ordering::Relaxed),
                    total: ctx.active_job.total.load(Ordering::Relaxed),
                })
            } else {
                None
            };
            let snap = DaemonResponse::StatusSnapshot { peers, uptime_secs, active_job, queue_len };
            let _ = send_response(&mut framed, &snap).await;
        }

        DaemonRequest::Logs { lines } => {
            // Replay backlog first.
            let backlog: Vec<LogLine> = {
                let buf = ctx.ring_buf.read().await;
                let skip = buf.len().saturating_sub(lines);
                buf.iter().skip(skip).cloned().collect()
            };
            for log in backlog {
                if send_response(&mut framed, &DaemonResponse::from(log)).await.is_err() {
                    return;
                }
            }

            // Then stream live.
            let mut rx = ctx.event_tx.subscribe();
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Ok(resp @ DaemonResponse::LogLine { .. }) => {
                                if send_response(&mut framed, &resp).await.is_err() {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                debug!("daemon: log client lagged, dropped {n} records");
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                    _ = ctx.token.cancelled() => break,
                }
            }
        }

        DaemonRequest::Shutdown => {
            let _ = send_response(&mut framed, &DaemonResponse::ShutdownComplete).await;
            ctx.token.cancel();
        }
    }
}

async fn send_response<S>(
    framed: &mut Framed<S, LinesCodec>,
    resp: &DaemonResponse,
) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures::SinkExt;
    let line = serde_json::to_string(resp)?;
    framed.send(line).await.context("failed to send response to client")
}

// ── DaemonLogLayer ────────────────────────────────────────────────────────────

pub struct DaemonLogLayer {
    ring_buf: Arc<RwLock<VecDeque<LogLine>>>,
    event_tx: broadcast::Sender<DaemonResponse>,
    log_file: Option<Arc<tokio::sync::Mutex<BufWriter<std::fs::File>>>>,
}

impl DaemonLogLayer {
    pub fn new(
        ring_buf: Arc<RwLock<VecDeque<LogLine>>>,
        event_tx: broadcast::Sender<DaemonResponse>,
        log_path: Option<&Path>,
    ) -> Self {
        let log_file = log_path.and_then(|p| {
            OpenOptions::new()
                .append(true)
                .create(true)
                .open(p)
                .ok()
                .map(|f| Arc::new(tokio::sync::Mutex::new(BufWriter::new(f))))
        });
        Self { ring_buf, event_tx, log_file }
    }
}

impl<S: tracing::Subscriber> Layer<S> for DaemonLogLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level = event.metadata().level().to_string().to_uppercase();
        let ts = {
            let secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // Simple RFC-3339-ish timestamp without chrono dependency.
            format_unix_ts(secs)
        };

        let mut visitor = MsgVisitor(String::new());
        event.record(&mut visitor);
        let msg = visitor.0;

        let line = LogLine { ts: ts.clone(), level: level.clone(), msg: msg.clone() };
        let formatted = format!("{ts}  {level}  {msg}\n");

        // Ring buffer (sync write via blocking).
        {
            if let Ok(mut buf) = self.ring_buf.try_write() {
                if buf.len() >= RING_BUF_CAP {
                    buf.pop_front();
                }
                buf.push_back(line.clone());
            }
        }

        // Broadcast (ignore if no subscribers).
        let _ = self.event_tx.send(DaemonResponse::from(line));

        // Log file (best-effort sync write via try_lock).
        if let Some(file) = &self.log_file {
            if let Ok(mut guard) = file.try_lock() {
                let _ = guard.write_all(formatted.as_bytes());
                let _ = guard.flush();
            }
        }
    }
}

struct MsgVisitor(String);

impl tracing::field::Visit for MsgVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else if !self.0.is_empty() {
            self.0.push_str(&format!(" {field}={value}"));
        } else {
            self.0 = format!("{field}={value}");
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{value:?}");
        } else if !self.0.is_empty() {
            self.0.push_str(&format!(" {field}={value:?}"));
        } else {
            self.0 = format!("{field}={value:?}");
        }
    }
}

fn format_unix_ts(secs: u64) -> String {
    // Minimal RFC-3339 formatter without chrono.
    let s = secs;
    let sec = s % 60;
    let min = (s / 60) % 60;
    let hour = (s / 3600) % 24;
    let days = s / 86400;
    // Approximate date from days since epoch (good enough for log timestamps).
    let year = 1970 + days / 365;
    let day_of_year = days % 365;
    let month = day_of_year / 30 + 1;
    let day = day_of_year % 30 + 1;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

// ── PID file ──────────────────────────────────────────────────────────────────

pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    /// Acquire a PID file at `path`.
    ///
    /// If a stale PID file exists (process dead), it is removed first.
    /// Returns an error if a live process already owns the PID.
    pub fn acquire(path: PathBuf) -> Result<Self> {
        if path.exists() {
            let existing = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok());
            if let Some(pid) = existing {
                if is_process_alive(pid) {
                    anyhow::bail!("daemon already running (pid {pid})");
                }
                warn!("daemon: removing stale pid file (pid {pid} is dead)");
                let _ = std::fs::remove_file(&path);
            }
        }

        let pid = std::process::id();
        std::fs::write(&path, format!("{pid}\n")).context("failed to write pid file")?;
        Ok(Self { path })
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Check if a process with the given PID is alive using `kill(pid, 0)`.
pub fn is_process_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Read the PID from a PID file if it exists and the process is alive.
pub fn read_live_pid(pid_path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(pid_path).ok()?;
    let pid: u32 = content.trim().parse().ok()?;
    if is_process_alive(pid) { Some(pid) } else { None }
}

// ── Daemon spawn ──────────────────────────────────────────────────────────────

pub const DAEMON_CHILD_ENV: &str = "SCOPENODE_DAEMON_CHILD";

/// Spawn the daemon by re-executing the current binary with `SCOPENODE_DAEMON_CHILD=1`.
///
/// Uses `setsid()` in `pre_exec` to detach from the terminal. The parent returns
/// immediately after spawning — it does not wait for the child.
pub fn spawn_daemon(data_dir: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to find current executable")?;
    let log_path = data_dir.join("daemon.log");
    let log_file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log: {}", log_path.display()))?;

    let devnull = OpenOptions::new()
        .read(true)
        .open("/dev/null")
        .context("failed to open /dev/null")?;

    use std::os::unix::io::IntoRawFd;
    let log_fd = log_file.into_raw_fd();
    let null_fd = devnull.into_raw_fd();

    let mut cmd = std::process::Command::new(exe);
    cmd.env(DAEMON_CHILD_ENV, "1");
    // Pass the data dir so the daemon child knows where to bind its socket.
    cmd.env("SCOPENODE_DAEMON_DATA_DIR", data_dir);

    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            libc::setsid();
            // Redirect stdin to /dev/null, stdout+stderr to log file.
            libc::dup2(null_fd, 0);
            libc::dup2(log_fd, 1);
            libc::dup2(log_fd, 2);
            libc::close(null_fd);
            libc::close(log_fd);
            Ok(())
        });
    }

    cmd.spawn().context("failed to spawn daemon child process")?;
    Ok(())
}

// ── Client connect helper ─────────────────────────────────────────────────────

/// Connect to the daemon socket, returning a framed stream.
pub async fn connect_daemon(data_dir: &Path) -> Result<Framed<UnixStream, LinesCodec>> {
    let sock = socket_path(data_dir);
    let stream = UnixStream::connect(&sock)
        .await
        .with_context(|| format!("failed to connect to daemon socket: {}", sock.display()))?;
    Ok(Framed::new(stream, LinesCodec::new_with_max_length(65_536)))
}

/// Send a request and return the framed stream for reading responses.
pub async fn send_request(
    framed: &mut Framed<UnixStream, LinesCodec>,
    req: &DaemonRequest,
) -> Result<()> {
    use futures::SinkExt;
    let line = serde_json::to_string(req)?;
    framed.send(line).await.context("failed to send request to daemon")
}

/// Parse a single response line.
pub fn parse_response(line: &str) -> Result<DaemonResponse> {
    serde_json::from_str(line).context("failed to parse daemon response")
}

// ── Stale socket detection ────────────────────────────────────────────────────

/// Check whether the daemon is reachable via its socket (fast ping).
pub async fn is_daemon_reachable(data_dir: &Path) -> bool {
    let Ok(mut framed) = connect_daemon(data_dir).await else { return false };
    let Ok(_) = send_request(&mut framed, &DaemonRequest::Status).await else { return false };
    use tokio_stream::StreamExt;
    tokio::time::timeout(Duration::from_millis(500), framed.next())
        .await
        .map(|r| r.is_some_and(|r| r.is_ok()))
        .unwrap_or(false)
}

/// Remove stale daemon files (pid file for dead process, unreachable socket).
pub async fn cleanup_stale_files(data_dir: &Path) {
    let pid_p = pid_path(data_dir);
    let sock_p = socket_path(data_dir);

    if pid_p.exists() {
        let alive = std::fs::read_to_string(&pid_p)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .map(is_process_alive)
            .unwrap_or(false);
        if !alive {
            warn!("daemon: removing stale pid file");
            let _ = std::fs::remove_file(&pid_p);
        }
    }

    if sock_p.exists() && !is_daemon_reachable(data_dir).await {
        warn!("daemon: removing stale socket");
        let _ = std::fs::remove_file(&sock_p);
    }
}

// ── DaemonBoot ────────────────────────────────────────────────────────────────

/// The daemon's boot sequence and job runner loop.
///
/// Called by the daemon child process (detected via `SCOPENODE_DAEMON_CHILD` env var).
pub struct DaemonBoot;

impl DaemonBoot {
    /// Run the full daemon lifecycle:
    /// 1. Clean up stale socket/pid files
    /// 2. Bind Unix socket (before peer discovery so clients can connect immediately)
    /// 3. Acquire PID file
    /// 4. Set up tracing with `DaemonLogLayer`
    /// 5. Spawn accept loop
    /// 6. Start `DevP2PNetwork` (blocks until first peer in background)
    /// 7. Job runner loop
    /// 8. Graceful shutdown
    pub async fn run(data_dir: PathBuf) -> Result<()> {
        cleanup_stale_files(&data_dir).await;

        let sock_path = socket_path(&data_dir);
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path)
            .with_context(|| format!("failed to bind daemon socket: {}", sock_path.display()))?;

        let _pid_file = PidFile::acquire(pid_path(&data_dir))?;

        let (job_tx, job_rx) = mpsc::channel::<SyncJobRequest>(32);
        let (event_tx, _) = broadcast::channel::<DaemonResponse>(1024);
        let ring_buf: Arc<RwLock<VecDeque<LogLine>>> = Arc::new(RwLock::new(VecDeque::new()));
        let token = tokio_util::sync::CancellationToken::new();
        let peer_count = Arc::new(AtomicU32::new(0));
        let job_counter = Arc::new(AtomicU64::new(0));
        let job_queue_len = Arc::new(AtomicU64::new(0));
        let active_job = Arc::new(ActiveJobState::default());

        // Install tracing subscriber with both fmt layer (→ daemon.log via stdio redirect)
        // and DaemonLogLayer (→ ring buffer + broadcast for `sn logs`).
        let log_layer = DaemonLogLayer::new(
            Arc::clone(&ring_buf),
            event_tx.clone(),
            Some(&data_dir.join("daemon.log")),
        );
        use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};
        tracing_subscriber::registry()
            .with(fmt::layer().with_target(false))
            .with(log_layer)
            .init();

        tracing::info!("daemon starting — data_dir={}", data_dir.display());

        let server = DaemonServer::new(
            listener,
            job_tx,
            event_tx.clone(),
            Arc::clone(&ring_buf),
            token.clone(),
            Arc::clone(&peer_count),
            Arc::clone(&job_counter),
            Arc::clone(&job_queue_len),
            Arc::clone(&active_job),
        );
        let tracker = tokio_util::task::TaskTracker::new();
        tracker.spawn(server.accept_loop());

        // Start DevP2PNetwork in a background task — blocks until first peer.
        let (net_tx, net_rx) = tokio::sync::oneshot::channel();
        let data_dir_clone = data_dir.clone();
        tokio::spawn(async move {
            match crate::network::DevP2PNetwork::start(&data_dir_clone).await {
                Ok(net) => {
                    let _ = net_tx.send(Arc::new(net));
                }
                Err(e) => {
                    tracing::error!("devp2p startup failed: {e:#}");
                }
            }
        });

        tracing::info!("daemon socket ready — waiting for devp2p peers...");

        let network = net_rx
            .await
            .context("devp2p task exited without providing network")?;
        {
            use crate::network::EthNetwork;
            peer_count.store(network.peer_count().await as u32, Ordering::Relaxed);
        }
        tracing::info!("daemon ready — {} peer(s) connected", peer_count.load(Ordering::Relaxed));

        let db = scopenode_storage::Db::open(data_dir.join("scopenode.db"))
            .await
            .context("daemon: failed to open database")?;

        Self::job_runner(
            job_rx,
            network,
            db,
            event_tx,
            active_job,
            job_queue_len,
            token.clone(),
        )
        .await;

        tracker.close();
        tracker.wait().await;
        let _ = std::fs::remove_file(&sock_path);
        tracing::info!("daemon stopped");
        Ok(())
    }

    async fn job_runner(
        mut job_rx: mpsc::Receiver<SyncJobRequest>,
        network: Arc<crate::network::DevP2PNetwork>,
        db: scopenode_storage::Db,
        event_tx: broadcast::Sender<DaemonResponse>,
        active_job: Arc<ActiveJobState>,
        job_queue_len: Arc<AtomicU64>,
        token: tokio_util::sync::CancellationToken,
    ) {
        loop {
            tokio::select! {
                job = job_rx.recv() => {
                    let Some(job) = job else { break };
                    let _ = job_queue_len.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                        Some(v.saturating_sub(1))
                    });
                    active_job.active.store(true, Ordering::Relaxed);
                    active_job.job_id.store(job.job_id, Ordering::Relaxed);
                    active_job.stage.store(1, Ordering::Relaxed);
                    active_job.pos.store(0, Ordering::Relaxed);
                    active_job.total.store(0, Ordering::Relaxed);

                    tracing::info!("daemon: starting job {} — config={}", job.job_id, job.config_path);
                    let result = run_job(&job, &network, db.clone(), event_tx.clone(), Arc::clone(&active_job)).await;

                    active_job.active.store(false, Ordering::Relaxed);

                    if let Err(e) = result {
                        let msg = format!("{e:#}");
                        tracing::error!("daemon: job {} failed: {msg}", job.job_id);
                        let _ = event_tx.send(DaemonResponse::Error { msg });
                    }
                }
                _ = token.cancelled() => break,
            }
        }
    }
}

async fn run_job(
    job: &SyncJobRequest,
    network: &Arc<crate::network::DevP2PNetwork>,
    db: scopenode_storage::Db,
    event_tx: broadcast::Sender<DaemonResponse>,
    active_job: Arc<ActiveJobState>,
) -> Result<()> {
    use crate::config::{Config, parse_blocks_flag};
    use crate::pipeline::Pipeline;
    use indicatif::{MultiProgress, ProgressDrawTarget};

    let mut config = Config::from_file(std::path::Path::new(&job.config_path))
        .with_context(|| format!("failed to load config: {}", job.config_path))?;

    if let Some(ref range) = job.blocks_override {
        let (from, to) = parse_blocks_flag(range)
            .map_err(|e| anyhow::anyhow!("invalid blocks override \"{range}\": {e}"))?;
        for contract in &mut config.contracts {
            contract.from_block = from;
            contract.to_block = to;
        }
    }

    let _ = event_tx.send(DaemonResponse::Progress {
        job_id: job.job_id,
        stage: 1,
        pos: 0,
        total: 1,
        msg: "Starting sync pipeline...".into(),
    });
    active_job.stage.store(1, Ordering::Relaxed);

    let progress = MultiProgress::new();
    progress.set_draw_target(ProgressDrawTarget::hidden());

    let mut pipeline = Pipeline::new(config.clone(), Arc::clone(network), db.clone());
    pipeline.run(false, &progress).await.context("pipeline failed")?;

    // Count events stored for this job's contracts.
    let mut events_found: u64 = 0;
    for contract in &config.contracts {
        let addr = contract.address.to_checksum(None);
        if let Ok(n) = db.count_events_for_contract(&addr).await {
            events_found += n as u64;
        }
    }

    let _ = event_tx.send(DaemonResponse::JobComplete { job_id: job.job_id, events_found });
    tracing::info!("daemon: job {} complete — {events_found} events", job.job_id);
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::time::timeout;

    // ── Protocol round-trip ────────────────────────────────────────────────────

    #[test]
    fn sync_job_round_trips() {
        let req = DaemonRequest::SyncJob {
            config_path: "/foo/bar.toml".into(),
            blocks_override: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: DaemonRequest = serde_json::from_str(&json).unwrap();
        let DaemonRequest::SyncJob { config_path, blocks_override } = back else {
            panic!("wrong variant");
        };
        assert_eq!(config_path, "/foo/bar.toml");
        assert!(blocks_override.is_none());
    }

    #[test]
    fn job_accepted_round_trips() {
        let resp = DaemonResponse::JobAccepted { job_id: 1, queue_position: 1 };
        let json = serde_json::to_string(&resp).unwrap();
        let back: DaemonResponse = serde_json::from_str(&json).unwrap();
        let DaemonResponse::JobAccepted { job_id, queue_position } = back else {
            panic!("wrong variant");
        };
        assert_eq!(job_id, 1);
        assert_eq!(queue_position, 1);
    }

    #[test]
    fn shutdown_serialises_with_no_extra_fields() {
        let req = DaemonRequest::Shutdown;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"type":"Shutdown"}"#);
    }

    // ── PID file ───────────────────────────────────────────────────────────────

    #[test]
    fn pidfile_creates_and_drops() {
        let dir = TempDir::new().unwrap();
        let path = pid_path(dir.path());
        {
            let _pf = PidFile::acquire(path.clone()).unwrap();
            assert!(path.exists());
            let content = std::fs::read_to_string(&path).unwrap();
            let pid: u32 = content.trim().parse().unwrap();
            assert_eq!(pid, std::process::id());
        }
        assert!(!path.exists(), "pid file should be removed on drop");
    }

    #[test]
    fn pidfile_overwrites_stale() {
        let dir = TempDir::new().unwrap();
        let path = pid_path(dir.path());
        // Write a PID that definitely doesn't exist.
        std::fs::write(&path, "999999\n").unwrap();
        let pf = PidFile::acquire(path.clone());
        assert!(pf.is_ok(), "should succeed with stale pid");
        let content = std::fs::read_to_string(&path).unwrap();
        let pid: u32 = content.trim().parse().unwrap();
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn is_process_alive_self() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn is_process_alive_nonexistent() {
        assert!(!is_process_alive(999_999));
    }

    // ── Socket server ──────────────────────────────────────────────────────────

    async fn make_server(dir: &Path) -> (DaemonServer, mpsc::Receiver<SyncJobRequest>) {
        let sock = socket_path(dir);
        let listener = UnixListener::bind(&sock).unwrap();
        let (job_tx, job_rx) = mpsc::channel(32);
        let (event_tx, _) = broadcast::channel(256);
        let ring_buf = Arc::new(RwLock::new(VecDeque::new()));
        let token = tokio_util::sync::CancellationToken::new();
        let peer_count = Arc::new(AtomicU32::new(3));
        let job_counter = Arc::new(AtomicU64::new(0));
        let job_queue_len = Arc::new(AtomicU64::new(0));
        let active_job = Arc::new(ActiveJobState::default());
        let server = DaemonServer::new(
            listener, job_tx, event_tx, ring_buf, token, peer_count,
            job_counter, job_queue_len, active_job,
        );
        (server, job_rx)
    }

    #[tokio::test]
    async fn sync_job_accepted() {
        let dir = TempDir::new().unwrap();
        let (server, mut job_rx) = make_server(dir.path()).await;
        let token = server.token.clone();
        tokio::spawn(server.accept_loop());

        let mut framed = connect_daemon(dir.path()).await.unwrap();
        send_request(&mut framed, &DaemonRequest::SyncJob {
            config_path: "/tmp/test.toml".into(),
            blocks_override: None,
        }).await.unwrap();

        use tokio_stream::StreamExt;
        let line = timeout(Duration::from_secs(2), framed.next())
            .await.unwrap().unwrap().unwrap();
        let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
        let DaemonResponse::JobAccepted { job_id, queue_position } = resp else {
            panic!("expected JobAccepted, got: {line}");
        };
        assert_eq!(job_id, 1);
        assert_eq!(queue_position, 1);

        let job = timeout(Duration::from_secs(1), job_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(job.config_path, "/tmp/test.toml");

        token.cancel();
    }

    #[tokio::test]
    async fn status_returns_snapshot() {
        let dir = TempDir::new().unwrap();
        let (server, _) = make_server(dir.path()).await;
        let token = server.token.clone();
        tokio::spawn(server.accept_loop());

        let mut framed = connect_daemon(dir.path()).await.unwrap();
        send_request(&mut framed, &DaemonRequest::Status).await.unwrap();

        use tokio_stream::StreamExt;
        let line = timeout(Duration::from_secs(2), framed.next())
            .await.unwrap().unwrap().unwrap();
        let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
        assert!(matches!(resp, DaemonResponse::StatusSnapshot { peers: 3, .. }));
        token.cancel();
    }

    #[tokio::test]
    async fn shutdown_completes() {
        let dir = TempDir::new().unwrap();
        let (server, _) = make_server(dir.path()).await;
        let token = server.token.clone();
        let handle = tokio::spawn(server.accept_loop());

        let mut framed = connect_daemon(dir.path()).await.unwrap();
        send_request(&mut framed, &DaemonRequest::Shutdown).await.unwrap();

        use tokio_stream::StreamExt;
        let line = timeout(Duration::from_secs(2), framed.next())
            .await.unwrap().unwrap().unwrap();
        let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
        assert!(matches!(resp, DaemonResponse::ShutdownComplete));

        timeout(Duration::from_secs(2), handle).await.unwrap().unwrap();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn logs_streams_backlog_then_live() {
        let dir = TempDir::new().unwrap();
        let sock = socket_path(dir.path());
        let listener = UnixListener::bind(&sock).unwrap();
        let (job_tx, _) = mpsc::channel(32);
        let (event_tx, _) = broadcast::channel(256);
        let ring_buf = Arc::new(RwLock::new(VecDeque::new()));
        {
            let mut buf = ring_buf.write().await;
            for i in 0..5u32 {
                buf.push_back(LogLine {
                    ts: "2026-01-01T00:00:00Z".into(),
                    level: "INFO".into(),
                    msg: format!("backlog {i}"),
                });
            }
        }
        let token = tokio_util::sync::CancellationToken::new();
        let server = DaemonServer::new(
            listener, job_tx, event_tx.clone(), Arc::clone(&ring_buf),
            token.clone(),
            Arc::new(AtomicU32::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(ActiveJobState::default()),
        );
        tokio::spawn(server.accept_loop());

        let mut framed = connect_daemon(dir.path()).await.unwrap();
        send_request(&mut framed, &DaemonRequest::Logs { lines: 3 }).await.unwrap();

        use tokio_stream::StreamExt;
        // Expect 3 backlog lines (last 3 of 5).
        for i in 2..5u32 {
            let line = timeout(Duration::from_secs(2), framed.next())
                .await.unwrap().unwrap().unwrap();
            let resp: DaemonResponse = serde_json::from_str(&line).unwrap();
            let DaemonResponse::LogLine { msg, .. } = resp else { panic!("expected LogLine") };
            assert!(msg.contains(&i.to_string()));
        }

        // Send a live log and check it arrives.
        event_tx.send(DaemonResponse::LogLine {
            ts: "2026-01-01T00:00:00Z".into(),
            level: "INFO".into(),
            msg: "live log".into(),
        }).unwrap();
        let line = timeout(Duration::from_secs(2), framed.next())
            .await.unwrap().unwrap().unwrap();
        assert!(line.contains("live log"));

        token.cancel();
    }

    #[tokio::test]
    async fn ring_buffer_cap() {
        let ring_buf: Arc<RwLock<VecDeque<LogLine>>> = Arc::new(RwLock::new(VecDeque::new()));
        let (event_tx, _) = broadcast::channel::<DaemonResponse>(16);
        let layer = DaemonLogLayer::new(Arc::clone(&ring_buf), event_tx, None);

        use tracing_subscriber::{registry, layer::SubscriberExt};
        let sub = registry().with(layer);
        let _guard = tracing::subscriber::set_default(sub);

        for i in 0..=10_000u32 {
            tracing::info!("log line {i}");
        }

        let buf = ring_buf.read().await;
        assert_eq!(buf.len(), RING_BUF_CAP, "ring buffer should be capped at {RING_BUF_CAP}");
    }
}
