//! Configuration types loaded from the TOML config file.
//!
//! All fields map 1:1 to the TOML structure. Unknown fields are rejected
//! (`deny_unknown_fields`) to catch typos early and prevent silent misconfigurations.
//!
//! # Example config
//! ```toml
//! [node]
//! port = 8545
//!
//! [[contracts]]
//! name = "Uniswap V3 USDC/ETH"
//! address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
//! events = ["Swap"]
//! from_block = "12.3M"   # or 12376729
//! ```

use crate::error::ConfigError;
use alloy_primitives::Address;
use serde::Deserialize;
use std::fmt;
use std::path::PathBuf;

/// Root configuration loaded from `config.toml`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Node-level settings (port, data directory, consensus RPCs, reorg buffer).
    pub node: NodeConfig,

    /// Optional local historical source used by `scopenode index`.
    #[serde(default)]
    pub source: Option<SourceConfig>,

    /// List of contracts and events to sync. At least one contract is required.
    pub contracts: Vec<ContractConfig>,
}

/// Local historical source configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Source adapter kind. The first supported source is ERA1.
    pub kind: SourceKind,

    /// Local path to a source directory.
    pub path: PathBuf,

    /// Optional network override used when source filenames do not carry one.
    pub network: Option<String>,
}

/// Supported historical source adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Era1,
}

impl SourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Era1 => "era1",
        }
    }
}

/// Settings that apply to the whole node instance.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// JSON-RPC server port. Default: 8545.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Directory for the SQLite database and other persistent state.
    ///
    /// Defaults to `~/.scopenode`. Tilde expansion is performed.
    pub data_dir: Option<PathBuf>,
}

/// Configuration for a single contract to sync.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractConfig {
    /// Optional human-readable label shown in progress output and `scopenode status`.
    pub name: Option<String>,

    /// The Ethereum contract address to watch.
    pub address: Address,

    /// List of event names to index (e.g. `["Swap", "Mint"]`).
    pub events: Vec<String>,

    /// First block to sync (inclusive).
    ///
    /// Accepts an integer (`12376729`) or human-readable shorthand:
    /// - `"16M"` → 16,000,000
    /// - `"16.5M"` → 16,500,000
    /// - `"12.3K"` → 12,300
    #[serde(deserialize_with = "deser_block_number")]
    pub from_block: u64,

    /// Last block to sync (inclusive).
    ///
    /// Omit for live sync — the pipeline runs historical sync then switches to
    /// live block-by-block processing. Accepts the same shorthand as `from_block`.
    #[serde(deserialize_with = "deser_opt_block_number", default)]
    pub to_block: Option<u64>,

    /// Path to a local ABI JSON file. Use when the contract is not verified on Sourcify.
    pub abi_override: Option<PathBuf>,

    /// Implementation address for proxy contracts (EIP-1967 or any proxy pattern).
    pub impl_address: Option<Address>,
}

impl Config {
    /// Load and validate a config from a TOML file.
    pub fn from_file(path: &std::path::Path) -> Result<Self, ConfigError> {
        let content =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_owned(), e))?;
        let config: Self = toml::from_str(&content).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for c in &self.contracts {
            if let Some(to) = c.to_block {
                if to < c.from_block {
                    return Err(ConfigError::InvalidRange {
                        from: c.from_block,
                        to,
                        address: c.address,
                    });
                }
            }
            if c.events.is_empty() {
                return Err(ConfigError::NoEvents(c.address));
            }
        }
        Ok(())
    }
}

// ─── Block number deserializers ───────────────────────────────────────────────

/// Deserialize a block number from either an integer or a shorthand string.
///
/// Accepts:
/// - Integer: `12376729`
/// - `"16M"` → 16,000,000
/// - `"16.5M"` → 16,500,000
/// - `"12.3K"` → 12,300
fn deser_block_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    d.deserialize_any(BlockVisitor)
}

/// Deserialize an optional block number (absent field = `None`).
fn deser_opt_block_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    d.deserialize_any(OptBlockVisitor)
}

struct BlockVisitor;

impl<'de> serde::de::Visitor<'de> for BlockVisitor {
    type Value = u64;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a block number (integer) or shorthand like \"16M\", \"16.5M\", \"12.3K\""
        )
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<u64, E> {
        Ok(v)
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<u64, E> {
        if v < 0 {
            Err(E::custom("block number cannot be negative"))
        } else {
            Ok(v as u64)
        }
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<u64, E> {
        parse_block_shorthand(v).map_err(E::custom)
    }
}

struct OptBlockVisitor;

impl<'de> serde::de::Visitor<'de> for OptBlockVisitor {
    type Value = Option<u64>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a block number, shorthand like \"16M\", or absent/null for live sync"
        )
    }

    fn visit_none<E: serde::de::Error>(self) -> Result<Option<u64>, E> {
        Ok(None)
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<Option<u64>, E> {
        Ok(None)
    }

    fn visit_some<D2: serde::Deserializer<'de>>(self, d2: D2) -> Result<Option<u64>, D2::Error> {
        deser_block_number(d2).map(Some)
    }

    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Option<u64>, E> {
        Ok(Some(v))
    }

    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Option<u64>, E> {
        if v < 0 {
            Err(E::custom("block number cannot be negative"))
        } else {
            Ok(Some(v as u64))
        }
    }

    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Option<u64>, E> {
        parse_block_shorthand(v).map(Some).map_err(E::custom)
    }
}

/// Parse a human-readable block number shorthand into a raw block number.
///
/// Supported suffixes:
/// - `M` — multiply by 1,000,000 (e.g. `"16M"` → 16,000,000; `"16.5M"` → 16,500,000)
/// - `K` — multiply by 1,000 (e.g. `"12.3K"` → 12,300)
///
/// Plain integers (as strings) are also accepted: `"12376729"` → 12,376,729.
///
/// # Errors
/// Returns an error string if the input cannot be parsed.
pub fn parse_block_shorthand(s: &str) -> Result<u64, String> {
    if let Some(rest) = s.strip_suffix('M') {
        let n: f64 = rest.parse().map_err(|_| {
            format!("invalid block shorthand \"{s}\": expected a number before 'M'")
        })?;
        if n < 0.0 {
            return Err(format!(
                "invalid block shorthand \"{s}\": negative block number"
            ));
        }
        Ok((n * 1_000_000.0).round() as u64)
    } else if let Some(rest) = s.strip_suffix('K') {
        let n: f64 = rest.parse().map_err(|_| {
            format!("invalid block shorthand \"{s}\": expected a number before 'K'")
        })?;
        if n < 0.0 {
            return Err(format!(
                "invalid block shorthand \"{s}\": negative block number"
            ));
        }
        Ok((n * 1_000.0).round() as u64)
    } else {
        s.parse::<u64>().map_err(|_| {
            format!("invalid block number \"{s}\": expected an integer or shorthand like \"16M\"")
        })
    }
}

/// Parse a `--blocks` range string into `(from_block, to_block)`.
///
/// Formats: `"16M:17M"` or `"16M:+1000"` (relative offset).
pub fn parse_blocks_flag(s: &str) -> Result<(u64, Option<u64>), String> {
    let (left, right) = s.split_once(':').ok_or_else(|| {
        format!("expected colon separator in \"{s}\" — use e.g. \"16M:17M\" or \"16M:+1000\"")
    })?;
    let from = parse_block_shorthand(left.trim())?;
    let right = right.trim();
    let to = if let Some(offset_str) = right.strip_prefix('+') {
        let offset: u64 = offset_str
            .parse()
            .map_err(|_| format!("invalid relative offset \"+{offset_str}\""))?;
        from.checked_add(offset)
            .ok_or_else(|| format!("block range overflow: {from} + {offset}"))?
    } else {
        parse_block_shorthand(right)?
    };
    if to < from {
        return Err(format!("to_block ({to}) must be >= from_block ({from})"));
    }
    Ok((from, Some(to)))
}

fn default_port() -> u16 {
    8545
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_integer_string() {
        assert_eq!(parse_block_shorthand("12376729").unwrap(), 12376729);
    }

    #[test]
    fn parse_whole_m() {
        assert_eq!(parse_block_shorthand("16M").unwrap(), 16_000_000);
    }

    #[test]
    fn parse_decimal_m() {
        assert_eq!(parse_block_shorthand("16.5M").unwrap(), 16_500_000);
    }

    #[test]
    fn parse_k() {
        assert_eq!(parse_block_shorthand("12.3K").unwrap(), 12_300);
    }

    #[test]
    fn parse_unknown_suffix_errors() {
        assert!(parse_block_shorthand("16B").is_err());
    }

    #[test]
    fn parse_negative_errors() {
        assert!(parse_block_shorthand("-1M").is_err());
    }

    #[test]
    fn toml_integer_from_block() {
        let toml = r#"
            [node]
            [[contracts]]
            address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            events = ["Transfer"]
            from_block = 12376729
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.contracts[0].from_block, 12376729);
    }

    #[test]
    fn toml_shorthand_from_block() {
        let toml = r#"
            [node]
            [[contracts]]
            address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            events = ["Transfer"]
            from_block = "16M"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.contracts[0].from_block, 16_000_000);
    }

    #[test]
    fn toml_shorthand_decimal() {
        let toml = r#"
            [node]
            [[contracts]]
            address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            events = ["Transfer"]
            from_block = "16.5M"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.contracts[0].from_block, 16_500_000);
    }

    fn minimal_contract_toml() -> &'static str {
        r#"
            address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            events = ["Transfer"]
            from_block = 1
        "#
    }

    #[test]
    fn source_is_optional_for_existing_configs() {
        let toml = format!("[node]\n[[contracts]]\n{}", minimal_contract_toml());
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert!(cfg.source.is_none());
        cfg.validate().unwrap();
    }

    #[test]
    fn source_config_deserializes() {
        let toml = format!(
            r#"[node]

[source]
kind = "era1"
path = "fixtures/era1/mainnet"
network = "mainnet"

[[contracts]]
{}"#,
            minimal_contract_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        let source = cfg.source.unwrap();
        assert_eq!(source.kind, SourceKind::Era1);
        assert_eq!(source.path, PathBuf::from("fixtures/era1/mainnet"));
        assert_eq!(source.network.as_deref(), Some("mainnet"));
    }
}
