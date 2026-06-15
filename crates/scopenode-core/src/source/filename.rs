//! Archive filename parsing: format, network, epoch, hash, and the derived
//! block range encoded in an ERA1/ERE filename.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Era1FileName {
    pub(super) format: String,
    pub(super) network: String,
    pub(super) epoch: u64,
    pub(super) file_hash: String,
    pub(super) from_block: u64,
    pub(super) to_block: u64,
}

pub(super) fn parse_era1_filename(
    path: &Path,
    network_override: Option<&str>,
) -> Option<Era1FileName> {
    let extension = path.extension().and_then(|ext| ext.to_str())?;
    if extension != "era1" && extension != "ere" {
        return None;
    }

    let stem = path.file_stem()?.to_str()?;
    let parts = stem.split('-').collect::<Vec<_>>();
    if (extension == "era1" && parts.len() != 3) || (extension == "ere" && parts.len() < 3) {
        return None;
    }

    let network = network_override.unwrap_or(parts[0]).to_string();
    let epoch = parts[1].parse::<u64>().ok()?;
    let file_hash = parts[2].to_string();
    if file_hash.len() != 8 || !file_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    if extension == "ere"
        && !parts[3..]
            .iter()
            .all(|profile| !profile.is_empty() && profile.chars().all(|c| c.is_ascii_lowercase()))
    {
        return None;
    }
    let from_block = epoch.checked_mul(super::ERA1_BLOCKS_PER_FILE)?;
    let to_block = from_block.checked_add(super::ERA1_BLOCKS_PER_FILE - 1)?;

    Some(Era1FileName {
        format: extension.to_string(),
        network,
        epoch,
        file_hash,
        from_block,
        to_block,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_standard_era1_filename() {
        let parsed = parse_era1_filename(Path::new("mainnet-00000-5ec1ffb8.era1"), None).unwrap();

        assert_eq!(parsed.network, "mainnet");
        assert_eq!(parsed.epoch, 0);
        assert_eq!(parsed.file_hash, "5ec1ffb8");
        assert_eq!(parsed.from_block, 0);
        assert_eq!(parsed.to_block, 8191);
    }

    #[test]
    fn parses_standard_ere_filename_with_profile() {
        let parsed =
            parse_era1_filename(Path::new("mainnet-00012-4bb7de2e-noproofs.ere"), None).unwrap();

        assert_eq!(parsed.network, "mainnet");
        assert_eq!(parsed.epoch, 12);
        assert_eq!(parsed.file_hash, "4bb7de2e");
        assert_eq!(parsed.from_block, 98_304);
        assert_eq!(parsed.to_block, 106_495);
    }

    #[test]
    fn rejects_unrelated_extension() {
        assert!(parse_era1_filename(Path::new("notes.txt"), None).is_none());
    }

    #[test]
    fn rejects_era1_filename_with_wrong_part_count() {
        assert!(parse_era1_filename(Path::new("mainnet-00000.era1"), None).is_none());
    }

    #[test]
    fn rejects_filename_with_invalid_hex_hash() {
        assert!(parse_era1_filename(Path::new("mainnet-00000-not_a_hash.era1"), None).is_none());
    }

    #[test]
    fn rejects_ere_filename_with_invalid_profile_characters() {
        assert!(
            parse_era1_filename(Path::new("mainnet-00012-4bb7de2e-NoProofs.ere"), None).is_none()
        );
    }

    #[test]
    fn network_override_replaces_parsed_network() {
        let parsed =
            parse_era1_filename(Path::new("mainnet-00000-5ec1ffb8.era1"), Some("sepolia")).unwrap();

        assert_eq!(parsed.network, "sepolia");
    }
}
