//! Configuration types loaded from the TOML config file.
//!
//! All fields map 1:1 to the TOML structure. Unknown fields are rejected
//! (`deny_unknown_fields`) to catch typos early and prevent silent misconfigurations.
//!
//! # Example config
//! ```toml
//! [node]
//! port = 8545
//! rest_port = 8546
//! data_dir = "~/.scopenode"
//! era_dir = "~/era1"
//!
//! [[contracts]]
//! address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
//! events = ["Swap"]
//! from_block = "25M"
//! to_block = "25.01M"
//! abi_override = "./abis/UniswapV3Pool.json"
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
    /// Node-level settings (port, data directory, era directory).
    pub node: NodeConfig,

    /// List of contracts and events to sync. At least one contract is required.
    pub contracts: Vec<ContractConfig>,
}

/// Settings that apply to the whole node instance.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// JSON-RPC server port. Default: 8545 (the standard Ethereum RPC port).
    #[serde(default = "default_port")]
    pub port: u16,

    /// REST API server port. Default: 8546.
    #[serde(default = "default_rest_port")]
    pub rest_port: u16,

    /// Directory for the SQLite database and other persistent state.
    ///
    /// Defaults to `~/.scopenode`. Tilde expansion is performed by the caller.
    pub data_dir: Option<PathBuf>,

    /// Directory containing ERA1 files.
    ///
    /// Tilde expansion is performed by the caller.
    pub era_dir: PathBuf,
}

/// Configuration for a single contract to sync.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractConfig {
    /// Optional human-readable label shown in progress output.
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

    /// Last block to sync (inclusive). Required for ERA1 sync.
    ///
    /// Accepts the same shorthand as `from_block`.
    #[serde(deserialize_with = "deser_opt_block_number", default)]
    pub to_block: Option<u64>,

    /// Path to a local ABI JSON file. Required — remote ABI fetching is not supported.
    pub abi_override: Option<PathBuf>,
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
            if c.abi_override.is_none() {
                return Err(ConfigError::AbiOverrideRequired(c.address));
            }
            if c.to_block.is_none() {
                return Err(ConfigError::ToBlockRequired(c.address));
            }
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

fn deser_block_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    d.deserialize_any(BlockVisitor)
}

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
            "a block number, shorthand like \"16M\", or absent/null"
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

fn default_rest_port() -> u16 {
    8546
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_contract_toml() -> &'static str {
        r#"
            address = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            events = ["Transfer"]
            from_block = 1
            to_block = 100
            abi_override = "./abi.json"
        "#
    }

    fn minimal_node_toml() -> &'static str {
        r#"era_dir = "/tmp/era1""#
    }

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
        let toml = format!(
            "[node]\n{}\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = 12376729\nto_block = 12376800\nabi_override = \"./abi.json\"\n",
            minimal_node_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.contracts[0].from_block, 12376729);
    }

    #[test]
    fn toml_shorthand_from_block() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = \"16M\"\nto_block = \"16.1M\"\nabi_override = \"./abi.json\"\n",
            minimal_node_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.contracts[0].from_block, 16_000_000);
    }

    #[test]
    fn toml_shorthand_decimal() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = \"16.5M\"\nto_block = \"17M\"\nabi_override = \"./abi.json\"\n",
            minimal_node_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.contracts[0].from_block, 16_500_000);
    }

    #[test]
    fn port_defaults_to_8545() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\n{}\n",
            minimal_node_toml(),
            minimal_contract_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.node.port, 8545);
    }

    #[test]
    fn rest_port_defaults_to_8546() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\n{}\n",
            minimal_node_toml(),
            minimal_contract_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.node.rest_port, 8546);
    }

    #[test]
    fn era_dir_required() {
        let toml = "[node]\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = 1\nto_block = 100\nabi_override = \"./abi.json\"\n";
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn abi_override_required_in_validate() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = 1\nto_block = 100\n",
            minimal_node_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::AbiOverrideRequired(_)),
            "expected AbiOverrideRequired, got {err}"
        );
    }

    #[test]
    fn to_block_required_in_validate() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = 1\nabi_override = \"./abi.json\"\n",
            minimal_node_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::ToBlockRequired(_)),
            "expected ToBlockRequired, got {err}"
        );
    }

    #[test]
    fn invalid_range_errors() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\naddress = \"0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48\"\nevents = [\"Transfer\"]\nfrom_block = 100\nto_block = 50\nabi_override = \"./abi.json\"\n",
            minimal_node_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::InvalidRange { .. }),
            "expected InvalidRange, got {err}"
        );
    }

    #[test]
    fn happy_path_valid_config() {
        let toml = format!(
            "[node]\n{}\n[[contracts]]\n{}\n",
            minimal_node_toml(),
            minimal_contract_toml()
        );
        let cfg: Config = toml::from_str(&toml).unwrap();
        cfg.validate().unwrap();
    }
}
