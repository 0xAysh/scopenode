//! CLI runtime context — centralises config loading, data directory resolution,
//! tilde expansion, directory creation, db path construction, and DB opening.
//!
//! All three commands (sync, serve, status) share this identical setup sequence.
//! [`RuntimeContext::load`] encapsulates it so each command's `execute()` function
//! reduces to a single `await` call rather than five repeated lines.

use anyhow::{Context, Result};
use scopenode_core::config::Config;
use scopenode_storage::Db;
use std::path::PathBuf;

/// Fully initialised runtime state that every command needs before it can work.
pub struct RuntimeContext {
    /// Parsed and validated configuration loaded from the TOML file.
    pub config: Config,
    /// Open database handle (migrations already applied).
    pub db: Db,
    /// Absolute path to the database file (useful for display in `status`).
    pub db_path: PathBuf,
}

impl RuntimeContext {
    /// Load config, resolve + create the data directory, and open the database.
    ///
    /// Steps performed in order:
    /// 1. Parse TOML config from `config_path`.
    /// 2. Resolve `data_dir`: use the value from config or fall back to
    ///    [`default_data_dir`].
    /// 3. Expand a leading `~/` in `data_dir` to the real home directory.
    /// 4. Create `data_dir` (and any missing parents) with
    ///    [`std::fs::create_dir_all`].
    /// 5. Build `db_path` as `<data_dir>/scopenode.db`.
    /// 6. Open the SQLite database at `db_path` (runs migrations).
    pub async fn load(config_path: PathBuf) -> Result<Self> {
        let config = Config::from_file(&config_path).context("Failed to load config")?;

        let data_dir = expand_tilde(
            config
                .node
                .data_dir
                .clone()
                .unwrap_or_else(default_data_dir),
        );
        std::fs::create_dir_all(&data_dir)
            .with_context(|| format!("Failed to create data dir: {}", data_dir.display()))?;

        let db_path = data_dir.join("scopenode.db");
        let db = Db::open(db_path.clone())
            .await
            .context("Failed to open database")?;

        Ok(Self {
            config,
            db,
            db_path,
        })
    }
}

/// Return the default data directory: `~/.scopenode`.
///
/// Falls back to `./.scopenode` if the home directory cannot be determined.
pub(crate) fn default_data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".scopenode")
}

/// Expand a leading `~/` in `path` to the user's home directory.
///
/// Paths that do not start with `~/` are returned unchanged (including absolute
/// paths and bare relative paths).
pub(crate) fn expand_tilde(path: PathBuf) -> PathBuf {
    if let Some(s) = path.to_str() {
        if let Some(stripped) = s.strip_prefix("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(stripped);
            }
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    const MINIMAL_CONFIG_TOML: &str = r#"
[node]
era_dir = "/tmp/era1"

[[contracts]]
address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
events = ["Transfer"]
from_block = 1
to_block = 100
abi_override = "./abi.json"
"#;

    // ── expand_tilde ──────────────────────────────────────────────────────────

    #[test]
    fn expand_tilde_replaces_tilde_prefix() {
        let home = dirs::home_dir().expect("home dir must exist for this test");
        let input = PathBuf::from("~/foo");
        let expanded = expand_tilde(input);
        assert_eq!(expanded, home.join("foo"));
    }

    #[test]
    fn expand_tilde_leaves_absolute_path_unchanged() {
        let input = PathBuf::from("/absolute/path");
        assert_eq!(expand_tilde(input.clone()), input);
    }

    #[test]
    fn expand_tilde_leaves_relative_path_unchanged() {
        let input = PathBuf::from("relative/path");
        assert_eq!(expand_tilde(input.clone()), input);
    }

    // ── default_data_dir ──────────────────────────────────────────────────────

    #[test]
    fn default_data_dir_ends_with_scopenode() {
        let dir = default_data_dir();
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(".scopenode"),
            "default data dir should end with .scopenode, got: {}",
            dir.display()
        );
    }

    // ── db_path construction ──────────────────────────────────────────────────

    #[test]
    fn db_path_is_scopenode_db_in_data_dir() {
        let data_dir = PathBuf::from("/some/data/dir");
        let db_path = data_dir.join("scopenode.db");
        assert_eq!(
            db_path.file_name().and_then(|n| n.to_str()),
            Some("scopenode.db")
        );
        assert_eq!(db_path.parent(), Some(data_dir.as_path()));
    }

    // ── RuntimeContext::load ──────────────────────────────────────────────────

    #[tokio::test]
    async fn load_creates_data_dir_and_opens_db() {
        let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");

        // Write a minimal config TOML that does NOT specify data_dir — we will
        // inject one via a second temp dir so the test is fully self-contained.
        let data_dir = tmp_dir.path().join("scopenode_data");
        let config_toml = format!(
            "[node]\nera_dir = \"/tmp/era1\"\ndata_dir = \"{}\"\n\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = 1\nto_block = 100\nabi_override = \"./abi.json\"\n",
            data_dir.display()
        );

        let mut config_file = tempfile::NamedTempFile::new_in(tmp_dir.path())
            .expect("failed to create temp config file");
        config_file
            .write_all(config_toml.as_bytes())
            .expect("failed to write config");
        // Keep the file open/alive through the test via the handle.
        let config_path = config_file.path().to_path_buf();

        let ctx = RuntimeContext::load(config_path)
            .await
            .expect("RuntimeContext::load should succeed");

        // Data dir must have been created.
        assert!(
            data_dir.exists(),
            "data_dir should have been created by RuntimeContext::load"
        );

        // db_path must end with "scopenode.db" and live inside data_dir.
        assert_eq!(
            ctx.db_path.file_name().and_then(|n| n.to_str()),
            Some("scopenode.db"),
            "db_path should end with scopenode.db"
        );
        assert_eq!(
            ctx.db_path.parent(),
            Some(data_dir.as_path()),
            "db_path should be inside data_dir"
        );
    }

    // Verify that load() also works when data_dir is NOT set in the config
    // (i.e. falls back to default_data_dir). We cannot easily test the exact
    // path without mutating HOME, so we only check that the call succeeds and
    // db_path has the right filename.
    #[tokio::test]
    async fn load_with_default_data_dir_opens_db() {
        // Write config without data_dir key — will use default (~/.scopenode).
        let tmp_dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut config_file = tempfile::NamedTempFile::new_in(tmp_dir.path())
            .expect("failed to create temp config file");
        config_file
            .write_all(MINIMAL_CONFIG_TOML.as_bytes())
            .expect("failed to write config");
        let config_path = config_file.path().to_path_buf();

        let ctx = RuntimeContext::load(config_path)
            .await
            .expect("RuntimeContext::load should succeed");

        assert_eq!(
            ctx.db_path.file_name().and_then(|n| n.to_str()),
            Some("scopenode.db")
        );
    }
}
