use scopenode_core::source::{read_era1_block_tuple, scan_era1_source, ChecksumStatus};
use std::path::PathBuf;

#[test]
fn scans_downloaded_era1_fixture_when_present() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/era1/mainnet");
    let fixture = root.join("mainnet-00000-5ec1ffb8.era1");
    if !fixture.exists() {
        eprintln!(
            "skipping ERA1 manifest fixture test: {} is missing",
            fixture.display()
        );
        return;
    }

    let scan = scan_era1_source(&root, None).unwrap();

    assert_eq!(scan.kind, "era1");
    assert_eq!(scan.network.as_deref(), Some("mainnet"));
    assert_eq!(scan.files.len(), 1);
    let file = &scan.files[0];
    assert_eq!(file.filename, "mainnet-00000-5ec1ffb8.era1");
    assert_eq!(file.network, "mainnet");
    assert_eq!(file.epoch, 0);
    assert_eq!(file.ranges[0].from_block, 0);
    assert_eq!(file.ranges[0].to_block, 8191);
    assert_eq!(file.checksum_status, ChecksumStatus::Verified);
}

#[test]
fn reads_first_block_tuple_from_downloaded_era1_fixture_when_present() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures/era1/mainnet");
    let fixture = root.join("mainnet-00000-5ec1ffb8.era1");
    if !fixture.exists() {
        eprintln!(
            "skipping ERA1 block tuple fixture test: {} is missing",
            fixture.display()
        );
        return;
    }

    let block = read_era1_block_tuple(&fixture, 0).unwrap().unwrap();

    assert_eq!(block.block_number, 0);
    assert!(!block.compressed_header.is_empty());
    assert!(!block.compressed_body.is_empty());
    assert!(!block.compressed_receipts.is_empty());
    assert!(!block.total_difficulty.is_empty());
}
