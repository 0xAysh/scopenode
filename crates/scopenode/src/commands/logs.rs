//! `logs` command — stream daemon log output.

use std::path::Path;

use anyhow::Result;
use scopenode_core::daemon::{
    connect_daemon, pid_path, read_live_pid, send_request, DaemonRequest, DaemonResponse,
};
use tokio_stream::StreamExt;

pub async fn run(data_dir: &Path, lines: usize) -> Result<()> {
    if read_live_pid(&pid_path(data_dir)).is_none() {
        anyhow::bail!("no daemon running — start it with `sn start`");
    }

    let mut framed = connect_daemon(data_dir).await?;
    send_request(&mut framed, &DaemonRequest::Logs { lines }).await?;

    println!("Attaching to daemon logs (last {lines} lines shown)...");

    loop {
        tokio::select! {
            msg = framed.next() => {
                match msg {
                    Some(Ok(line)) => {
                        if let Ok(resp) = serde_json::from_str::<DaemonResponse>(&line) {
                            if let DaemonResponse::LogLine { ts, level, msg } = resp {
                                println!("{ts}  {level}  {msg}");
                            }
                            // Non-LogLine messages are silently ignored.
                        }
                    }
                    Some(Err(_)) | None => break,
                }
            }
            _ = tokio::signal::ctrl_c() => {
                // Detach — job continues in daemon.
                break;
            }
        }
    }

    Ok(())
}
