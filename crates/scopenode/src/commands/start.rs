//! `start` command — spawn the daemon if not already running.

use std::path::Path;

use anyhow::Result;
use scopenode_core::daemon::{pid_path, read_live_pid, socket_path, spawn_daemon};

pub async fn run(data_dir: &Path) -> Result<()> {
    if let Some(pid) = read_live_pid(&pid_path(data_dir)) {
        anyhow::bail!("daemon already running (pid {pid})");
    }

    spawn_daemon(data_dir)?;

    println!("daemon started");
    println!("socket: {}", socket_path(data_dir).display());
    println!("log:    {}", data_dir.join("daemon.log").display());

    Ok(())
}
