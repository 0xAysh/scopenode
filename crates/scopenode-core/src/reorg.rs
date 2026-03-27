//! Reorg detection via a rolling `parent_hash` buffer.
//!
//! # How it works
//!
//! [`ReorgDetector`] maintains a sliding window of the last `reorg_buffer`
//! blocks (block number + block hash). On each new block it checks whether
//! `new_block.parent_hash` matches the tip of the window:
//!
//! - **Match** → normal chain extension, advance the window.
//! - **Mismatch** → reorg detected. Walk backward through the window to find
//!   the block whose hash equals `new_block.parent_hash` — that is the common
//!   ancestor. All window entries *after* it are orphaned.
//!
//! The caller receives a [`ReorgEvent`] listing the orphaned block hashes and
//! should pass them to [`crate::storage::Db::mark_reorged_by_hash`] so the
//! events stored from those blocks get `reorged = 1`.
//!
//! # Post-Merge safety
//!
//! After ~64 blocks (~12.8 min) the Ethereum beacon chain finalises a block
//! cryptographically — reorgs beyond that depth require breaking BLS-512
//! consensus, which is treated as impossible. The buffer capacity should
//! therefore be set to `reorg_buffer` from the node config (default 64).

use alloy_primitives::B256;
use std::collections::VecDeque;

/// A reorg that was detected while calling [`ReorgDetector::advance`].
#[derive(Debug, PartialEq)]
pub struct ReorgEvent {
    /// Block number of the most recent common ancestor (the last shared block).
    pub common_ancestor: u64,
    /// Hashes of the orphaned blocks — blocks we stored that are no longer on
    /// the canonical chain. Pass these to `Db::mark_reorged_by_hash`.
    pub orphaned_hashes: Vec<B256>,
}

/// Detects chain reorganisations using a rolling window of recent block hashes.
///
/// Feed every new block into [`advance`][Self::advance]. On startup, seed the
/// window from stored headers with [`seed`][Self::seed] so the detector can
/// catch reorgs that cross the restart boundary.
pub struct ReorgDetector {
    /// Sliding window of `(block_number, block_hash)` in ascending order.
    buffer: VecDeque<(u64, B256)>,
    capacity: usize,
}

impl ReorgDetector {
    /// Create a new detector with the given window size.
    ///
    /// `capacity` should equal `node.reorg_buffer` from the config (default 64).
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Seed the window from already-stored headers.
    ///
    /// Call this on startup after loading the last `reorg_buffer` headers from
    /// SQLite. Headers must be in ascending block-number order.
    pub fn seed(&mut self, headers: impl IntoIterator<Item = (u64, B256)>) {
        for (num, hash) in headers {
            if self.buffer.len() == self.capacity {
                self.buffer.pop_front();
            }
            self.buffer.push_back((num, hash));
        }
    }

    /// Current chain tip (the most recent block added to the window).
    pub fn tip(&self) -> Option<(u64, B256)> {
        self.buffer.back().copied()
    }

    /// Process the next block.
    ///
    /// Returns `None` on a normal chain extension.
    /// Returns `Some(ReorgEvent)` if the block's `parent_hash` does not match
    /// the expected tip, indicating a reorganisation.
    ///
    /// # Reorg deeper than the buffer
    ///
    /// Post-Merge this should never happen. If `parent_hash` is not found
    /// anywhere in the window the entire window is treated as orphaned, the
    /// buffer is cleared, and the new block becomes the sole entry.
    pub fn advance(&mut self, block_num: u64, block_hash: B256, parent_hash: B256) -> Option<ReorgEvent> {
        if self.buffer.is_empty() {
            self.buffer.push_back((block_num, block_hash));
            return None;
        }

        let &(tip_num, tip_hash) = self.buffer.back().unwrap();

        // Happy path: the new block extends our current tip.
        if block_num == tip_num + 1 && parent_hash == tip_hash {
            if self.buffer.len() == self.capacity {
                self.buffer.pop_front();
            }
            self.buffer.push_back((block_num, block_hash));
            return None;
        }

        // Reorg: find the common ancestor by locating parent_hash in the window.
        if let Some(pos) = self.buffer.iter().position(|&(_, h)| h == parent_hash) {
            let common_ancestor = self.buffer[pos].0;
            let orphaned_hashes: Vec<B256> = self.buffer
                .iter()
                .skip(pos + 1)
                .map(|&(_, h)| h)
                .collect();

            // Discard orphaned entries and add the new block.
            self.buffer.truncate(pos + 1);
            if self.buffer.len() == self.capacity {
                self.buffer.pop_front();
            }
            self.buffer.push_back((block_num, block_hash));

            return Some(ReorgEvent { common_ancestor, orphaned_hashes });
        }

        // parent_hash not in window — reorg deeper than buffer (should not happen post-Merge).
        let common_ancestor = self.buffer
            .front()
            .map(|&(n, _)| n.saturating_sub(1))
            .unwrap_or(0);
        let orphaned_hashes: Vec<B256> = self.buffer.iter().map(|&(_, h)| h).collect();
        self.buffer.clear();
        self.buffer.push_back((block_num, block_hash));

        Some(ReorgEvent { common_ancestor, orphaned_hashes })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    /// Build a deterministic chain: each block's hash is `keccak256(block_num)`,
    /// and its parent_hash is `keccak256(block_num - 1)` (or B256::ZERO for block 0).
    fn chain_hash(n: u64) -> B256 {
        keccak256(n.to_be_bytes())
    }

    /// Returns `(block_num, block_hash, parent_hash)` for a canonical chain.
    fn canonical(from: u64, to: u64) -> Vec<(u64, B256, B256)> {
        (from..=to)
            .map(|n| {
                let parent = if n == 0 { B256::ZERO } else { chain_hash(n - 1) };
                (n, chain_hash(n), parent)
            })
            .collect()
    }

    /// Returns a forked block at height `n` with a distinct hash.
    /// Its parent_hash still points to block `n-1` on the canonical chain,
    /// so the common ancestor is `n-1`.
    fn fork_block(n: u64) -> (u64, B256, B256) {
        let fork_hash = keccak256(format!("fork-{n}").as_bytes());
        let parent = if n == 0 { B256::ZERO } else { chain_hash(n - 1) };
        (n, fork_hash, parent)
    }

    #[test]
    fn no_reorg_linear_chain() {
        let mut d = ReorgDetector::new(64);
        for (num, hash, parent) in canonical(1, 100) {
            assert!(d.advance(num, hash, parent).is_none());
        }
        assert_eq!(d.tip(), Some((100, chain_hash(100))));
    }

    #[test]
    fn reorg_depth_1() {
        let mut d = ReorgDetector::new(64);
        // Build canonical blocks 1..=10.
        for (num, hash, parent) in canonical(1, 10) {
            d.advance(num, hash, parent);
        }

        // Block 10 is replaced by a fork at depth 1 (parent = block 9).
        let (num, fork_hash, parent) = fork_block(10);
        let event = d.advance(num, fork_hash, parent).expect("reorg expected");

        assert_eq!(event.common_ancestor, 9);
        assert_eq!(event.orphaned_hashes, vec![chain_hash(10)]);
        assert_eq!(d.tip(), Some((10, fork_hash)));
    }

    #[test]
    fn reorg_depth_5() {
        let mut d = ReorgDetector::new(64);
        for (num, hash, parent) in canonical(1, 20) {
            d.advance(num, hash, parent);
        }

        // Fork starts at block 16 (parent = block 15), displacing blocks 16-20.
        let (num, fork_hash, parent) = fork_block(16);
        let event = d.advance(num, fork_hash, parent).expect("reorg expected");

        assert_eq!(event.common_ancestor, 15);
        assert_eq!(
            event.orphaned_hashes,
            (16..=20).map(chain_hash).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reorg_depth_64() {
        let mut d = ReorgDetector::new(64);
        // Feed 200 canonical blocks — buffer stays at 64.
        for (num, hash, parent) in canonical(1, 200) {
            d.advance(num, hash, parent);
        }

        // Fork at block 137 (common ancestor = 136, depth = 64).
        let (num, fork_hash, parent) = fork_block(137);
        let event = d.advance(num, fork_hash, parent).expect("reorg expected");

        assert_eq!(event.common_ancestor, 136);
        assert_eq!(
            event.orphaned_hashes,
            (137..=200).map(chain_hash).collect::<Vec<_>>()
        );
    }

    #[test]
    fn reorg_deeper_than_buffer_orphans_all() {
        let mut d = ReorgDetector::new(10);
        for (num, hash, parent) in canonical(1, 10) {
            d.advance(num, hash, parent);
        }

        // A block with a completely unknown parent (deeper than the 10-block buffer).
        let unknown_parent = keccak256(b"unknown");
        let event = d
            .advance(11, keccak256(b"orphan-tip"), unknown_parent)
            .expect("reorg expected");

        // All 10 buffered blocks are orphaned.
        assert_eq!(event.orphaned_hashes.len(), 10);
        assert_eq!(event.common_ancestor, 0); // 1.saturating_sub(1)
    }

    #[test]
    fn seed_populates_buffer() {
        let mut d = ReorgDetector::new(64);
        // Seed with blocks 1..=5.
        d.seed((1..=5).map(|n| (n, chain_hash(n))));

        // Block 6 extends correctly — no reorg.
        let result = d.advance(6, chain_hash(6), chain_hash(5));
        assert!(result.is_none());
        assert_eq!(d.tip(), Some((6, chain_hash(6))));
    }

    #[test]
    fn seed_catches_reorg_after_restart() {
        let mut d = ReorgDetector::new(64);
        // Simulate restart: seed buffer with canonical blocks 1..=10.
        d.seed((1..=10).map(|n| (n, chain_hash(n))));

        // After restart, a fork at block 9 arrives (common ancestor = 8).
        let (num, fork_hash, parent) = fork_block(9);
        let event = d.advance(num, fork_hash, parent).expect("reorg expected");

        assert_eq!(event.common_ancestor, 8);
        assert_eq!(
            event.orphaned_hashes,
            vec![chain_hash(9), chain_hash(10)]
        );
    }

    #[test]
    fn buffer_slides_correctly() {
        let mut d = ReorgDetector::new(5);
        for (num, hash, parent) in canonical(1, 10) {
            d.advance(num, hash, parent);
        }
        // Buffer should hold blocks 6-10 only.
        assert_eq!(d.tip(), Some((10, chain_hash(10))));

        // A reorg targeting block 6 (which is still in the buffer).
        let (num, fork_hash, parent) = fork_block(6);
        let event = d.advance(num, fork_hash, parent).expect("reorg expected");

        assert_eq!(event.common_ancestor, 5);
        assert_eq!(
            event.orphaned_hashes,
            (6..=10).map(chain_hash).collect::<Vec<_>>()
        );
    }
}
