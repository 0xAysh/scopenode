//! `status` command — shows daemon state and indexed contracts.
//!
//! Queries the daemon socket when the daemon is running, then shows DB stats.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use scopenode_core::daemon::{
    connect_daemon, parse_response, pid_path, read_live_pid, send_request, DaemonRequest,
    DaemonResponse,
};
use scopenode_storage::Db;
use tokio_stream::StreamExt;

/// Print daemon state (if running) and indexed contracts from the database.
pub async fn run(db: Db, data_dir: &Path) -> Result<()> {
    print_daemon_status(data_dir).await;
    print_db_status(&db).await?;
    Ok(())
}

async fn print_daemon_status(data_dir: &Path) {
    let pid_p = pid_path(data_dir);

    // No PID file at all → not running.
    if read_live_pid(&pid_p).is_none() {
        if pid_p.exists() {
            // PID file exists but process is dead → starting up or crashed.
            println!("daemon  starting...");
        } else {
            println!("daemon  not running");
        }
        return;
    }

    // PID alive — try to connect to the socket.
    let mut framed = match connect_daemon(data_dir).await {
        Ok(f) => f,
        Err(_) => {
            println!("daemon  starting...");
            return;
        }
    };

    if send_request(&mut framed, &DaemonRequest::Status).await.is_err() {
        println!("daemon  unreachable");
        return;
    }

    let line = match tokio::time::timeout(Duration::from_millis(500), framed.next()).await {
        Ok(Some(Ok(l))) => l,
        _ => {
            println!("daemon  unreachable");
            return;
        }
    };

    let resp = match parse_response(&line) {
        Ok(r) => r,
        Err(_) => {
            println!("daemon  unreachable");
            return;
        }
    };

    if let DaemonResponse::StatusSnapshot { peers, uptime_secs, active_job, queue_len } = resp {
        let uptime = format_uptime(uptime_secs);
        print!("daemon  running  {peers} peers  uptime {uptime}");
        if queue_len > 0 {
            print!("  queue {queue_len}");
        }
        println!();
        if let Some(job) = active_job {
            println!(
                "        active job #{} — Stage {}/3  {}/{}",
                job.job_id, job.stage, job.pos, job.total
            );
        }
    }
}

async fn print_db_status(db: &Db) -> Result<()> {
    let contracts = db.get_all_contracts().await?;
    let pending_retry = db.count_pending_retry().await?;
    let db_size = db.db_size_bytes().await?;

    if contracts.is_empty() {
        println!("\nNo contracts indexed yet. Run `sn sync <config>` to start.");
        return Ok(());
    }

    println!("\nscopenode — indexed contracts\n");
    println!("{:<44}  {:<30}  Events", "Address", "Name");
    println!("{}", "─".repeat(80));

    for c in &contracts {
        let name = c.name.as_deref().unwrap_or("(unnamed)");
        let total: i64 = db.count_events_for_contract(&c.address).await?;
        println!("{:<44}  {:<30}  {}", c.address, name, total);
    }

    println!();
    println!("Database size:     {} KB", db_size / 1024);
    println!("Pending retry:     {pending_retry} blocks");

    Ok(())
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}
