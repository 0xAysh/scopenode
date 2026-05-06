//! `stop` command — send Shutdown to the daemon and wait for it to exit.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use scopenode_core::daemon::{
    connect_daemon, parse_response, pid_path, read_live_pid, send_request, DaemonRequest,
    DaemonResponse,
};
use tokio_stream::StreamExt;

pub async fn run(data_dir: &Path, force: bool) -> Result<()> {
    let pid = match read_live_pid(&pid_path(data_dir)) {
        Some(p) => p,
        None => {
            println!("no daemon running");
            return Ok(());
        }
    };

    if force {
        println!("sending SIGTERM to daemon (pid {pid})...");
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        println!("signal sent");
        return Ok(());
    }

    let mut framed = connect_daemon(data_dir).await?;
    send_request(&mut framed, &DaemonRequest::Shutdown).await?;

    let started = Instant::now();
    let mut last_print = Instant::now();

    loop {
        let timeout_result = tokio::time::timeout(Duration::from_secs(1), framed.next()).await;

        match timeout_result {
            Ok(Some(Ok(line))) => {
                let resp = parse_response(&line)?;
                match resp {
                    DaemonResponse::ShutdownComplete => {
                        println!("daemon stopped");
                        return Ok(());
                    }
                    DaemonResponse::Progress { stage, pos, total, .. } => {
                        println!("still running: Stage {stage}/3 — {pos}/{total} blocks");
                    }
                    _ => {}
                }
            }
            Ok(None) | Ok(Some(Err(_))) => {
                // Socket closed — daemon exited.
                println!("daemon stopped");
                return Ok(());
            }
            Err(_) => {
                // Timeout — check if 30s have passed since last message.
                if last_print.elapsed() >= Duration::from_secs(30) {
                    println!(
                        "still running ({}s elapsed) — use --force to kill immediately",
                        started.elapsed().as_secs()
                    );
                    last_print = Instant::now();
                }
            }
        }
    }
}
