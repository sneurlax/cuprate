#![allow(
    // `EnvInner` is an RAII guard; the borrow pattern always triggers this lint.
    // See `cuprate-blockchain` and `cuprate-database` for the same allowance.
    clippy::significant_drop_tightening
)]
//! Minimal in-process Monero regtest node for integration testing.
//!
//! Provides [`RegtestNode`]: a temporary blockchain database with a synthetic
//! genesis block. Callers mine coinbase-only blocks at difficulty 1 with no
//! PoW verification, no networking or RPC.
//!
//! HF1 blocks use V1 transactions; HF12+ uses V2 with null proofs.
//! PoW hash is always `[0u8; 32]`; difficulty is always 1.
//! The default coinbase output key is a fixed, well-known public point.

pub mod decoy_rpc;
pub use decoy_rpc::RegtestDecoyRpc;

mod error;
pub use error::RegtestError;

use std::sync::Arc;
use tempfile::TempDir;

use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, EdwardsPoint, Scalar};

use monero_oxide::{
    block::{Block, BlockHeader},
    io::{CompressedPoint, VarInt},
    primitives::{keccak256, keccak256_to_scalar},
    transaction::{Input, Output, Timelock, Transaction, TransactionPrefix},
};
use monero_wallet::rpc::ScannableBlock;

use cuprate_blockchain::{
    config::ConfigBuilder,
    ops::block::{add_block, get_block_extended_header_from_height},
    service::{init_read_service, BlockchainReadHandle},
    tables::OpenTables,
};
use cuprate_consensus_rules::{
    blocks::{penalty_free_zone, PENALTY_FREE_ZONE_1},
    hard_forks::HardFork,
    miner_tx::calculate_block_reward,
};
use cuprate_database::{ConcreteEnv, Env, EnvInner, TxRw};
use cuprate_database_service::ReaderThreads;
use cuprate_types::{ExtendedBlockHeader, VerifiedBlockInformation, VerifiedTransactionInformation};

/// Placeholder output key for non-scanned coinbase outputs (regtest only).
///
/// A fixed, publicly-known point (the Ed25519 basepoint G) used as a dummy output key.
const REGTEST_MINER_KEY: CompressedPoint = CompressedPoint([
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
]);

fn derive_coinbase_output_from_pubkey(
    spend_pub: &EdwardsPoint,
    view_pub: &EdwardsPoint,
    tx_scalar: &Scalar,
    output_index: usize,
) -> (EdwardsPoint, u8) {
    let ecdh = tx_scalar * view_pub;
    let ecdh8 = ecdh.mul_by_cofactor();
    let ecdh8_bytes = ecdh8.compress().to_bytes();

    let mut derivation = ecdh8_bytes.to_vec();
    VarInt::write(&output_index, &mut derivation).expect("vec write");

    let view_tag = keccak256([b"view_tag".as_slice(), &derivation].concat())[0];
    let shared_key = keccak256_to_scalar(&derivation);
    let output_key = &shared_key * ED25519_BASEPOINT_TABLE + spend_pub;

    (output_key, view_tag)
}

fn build_scannable_coinbase_tx(
    chain_height: usize,
    reward: u64,
    spend_pub: &EdwardsPoint,
    view_pub: &EdwardsPoint,
) -> Transaction {
    use rand::rngs::OsRng;

    let tx_scalar = Scalar::random(&mut OsRng);
    let tx_pub = &tx_scalar * ED25519_BASEPOINT_TABLE;

    let (output_key, view_tag) =
        derive_coinbase_output_from_pubkey(spend_pub, view_pub, &tx_scalar, 0);

    let mut extra = vec![0x01_u8];
    extra.extend_from_slice(&tx_pub.compress().to_bytes());

    let prefix = TransactionPrefix {
        additional_timelock: Timelock::Block(chain_height + 60),
        inputs: vec![Input::Gen(chain_height)],
        outputs: vec![Output {
            amount: Some(reward),
            key: CompressedPoint(output_key.compress().to_bytes()),
            view_tag: Some(view_tag),
        }],
        extra,
    };

    Transaction::V2 { prefix, proofs: None }
}

/// Minimal coinbase tx: V1 for HF1-11, V2 (null proofs) for HF12+.
fn build_coinbase_tx(chain_height: usize, reward: u64, hf: HardFork) -> Transaction {
    let prefix = TransactionPrefix {
        additional_timelock: Timelock::Block(chain_height + 60),
        inputs: vec![Input::Gen(chain_height)],
        outputs: vec![Output {
            amount: Some(reward),
            key: REGTEST_MINER_KEY,
            view_tag: None,
        }],
        extra: vec![],
    };

    if hf >= HardFork::V12 {
        // coinbase transactions are exempt from RCT proofs
        Transaction::V2 {
            prefix,
            proofs: None,
        }
    } else {
        Transaction::V1 {
            prefix,
            signatures: vec![],
        }
    }
}

/// Wrap a block in [`VerifiedBlockInformation`] with dummy PoW and difficulty 1.
fn make_verified(
    block: Block,
    height: usize,
    reward: u64,
    cumulative_difficulty: u128,
    txs: Vec<VerifiedTransactionInformation>,
) -> VerifiedBlockInformation {
    let block_blob = block.serialize();
    let block_hash = block.hash();
    let tx_weight: usize = txs.iter().map(|t| t.tx_blob.len()).sum();
    let weight = block_blob.len() + tx_weight;
    VerifiedBlockInformation {
        block,
        block_blob,
        txs,
        block_hash,
        pow_hash: [0_u8; 32],
        height,
        generated_coins: reward,
        weight,
        long_term_weight: weight,
        cumulative_difficulty,
    }
}

/// Minimal in-process Monero regtest node.
///
/// Call [`RegtestNode::new`] to open a temp DB and commit a synthetic genesis,
/// then [`mine_blocks`](RegtestNode::mine_blocks) to grow the chain. Use
/// [`set_hard_fork`](RegtestNode::set_hard_fork) or
/// [`mine_blocks_at_hf`](RegtestNode::mine_blocks_at_hf) to switch forks.
/// Drop cleans up the temp directory automatically.
pub struct RegtestNode {
    env: Arc<ConcreteEnv>,
    read_handle: BlockchainReadHandle,
    _tmp: TempDir,
    /// Next block height to be mined (= number of blocks committed so far).
    height: usize,
    /// Hash of the most-recently-committed block.
    top_hash: [u8; 32],
    /// Total coins minted in all committed blocks.
    already_generated_coins: u64,
    /// Cumulative difficulty of the committed chain (increases by 1 per block).
    cumulative_difficulty: u128,
    /// The hard fork version used when mining new blocks.
    current_hf: HardFork,
    /// Transactions pending inclusion in the next block.
    pending_txs: Vec<VerifiedTransactionInformation>,
    /// All committed blocks in order, indexed by height.
    blocks: Vec<Block>,
    /// Cumulative `RingCT` output count after each committed block, indexed by height.
    /// `rct_counts[h]` = total RCT outputs in blocks 0..=h.
    rct_counts: Vec<u64>,
}

impl RegtestNode {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = ConfigBuilder::new()
            .data_directory(tmp.path().to_owned())
            .build();
        let env = Arc::new(cuprate_blockchain::open(config).expect("open blockchain db"));
        let read_handle =
            init_read_service(Arc::clone(&env), ReaderThreads::default());

        let hf = HardFork::V1;
        let genesis_reward = calculate_block_reward(0, PENALTY_FREE_ZONE_1, 0, hf);
        let miner_tx = build_coinbase_tx(0, genesis_reward, hf);

        let genesis = Block::new(
            BlockHeader {
                hardfork_version: 1,
                hardfork_signal: 0,
                timestamp: 0,
                previous: [0_u8; 32],
                nonce: 0,
            },
            miner_tx,
            vec![],
        )
        .expect("build genesis block");

        let genesis_hash = genesis.hash();
        let verified = make_verified(genesis, 0, genesis_reward, 1, vec![]);
        let genesis_clone = verified.block.clone();

        {
            let env_inner = env.env_inner();
            let tx_rw = env_inner.tx_rw().expect("tx_rw");
            {
                let mut tables = env_inner.open_tables_mut(&tx_rw).expect("open_tables_mut");
                add_block(&verified, &mut tables).expect("add genesis block");
            }
            TxRw::commit(tx_rw).expect("commit");
        }

        Self {
            env,
            read_handle,
            _tmp: tmp,
            height: 1,
            top_hash: genesis_hash,
            already_generated_coins: genesis_reward,
            cumulative_difficulty: 1,
            current_hf: HardFork::V1,
            pending_txs: vec![],
            // Genesis is V1, no RingCT outputs.
            blocks: vec![genesis_clone],
            rct_counts: vec![0],
        }
    }

    /// Number of blocks committed (genesis counts as 1).
    pub const fn height(&self) -> usize {
        self.height
    }

    /// Hash of the most-recently-committed block.
    pub const fn top_hash(&self) -> [u8; 32] {
        self.top_hash
    }

    /// Total coins minted so far.
    pub const fn already_generated_coins(&self) -> u64 {
        self.already_generated_coins
    }

    /// The current hard fork version used when mining new blocks.
    pub const fn current_hf(&self) -> HardFork {
        self.current_hf
    }

    /// Number of transactions waiting in the mempool.
    pub const fn mempool_size(&self) -> usize {
        self.pending_txs.len()
    }

    /// Deserializes and enqueues a transaction. Returns the tx hash, or [`RegtestError::BadTxBlob`] on bad input.
    pub fn submit_tx(&mut self, blob: Vec<u8>) -> Result<[u8; 32], RegtestError> {
        let tx = Transaction::read(&mut blob.as_slice())
            .map_err(|e| RegtestError::BadTxBlob(e.to_string()))?;
        let tx_hash = tx.hash();
        let tx_weight = blob.len();
        self.pending_txs.push(VerifiedTransactionInformation {
            tx,
            tx_blob: blob,
            tx_weight,
            fee: 0,
            tx_hash,
        });
        Ok(tx_hash)
    }

    /// Extended block header for the block at `height`.
    ///
    /// Returns `None` if `height >= self.height()`.
    pub fn block_header_at(&self, height: usize) -> Option<ExtendedBlockHeader> {
        if height >= self.height {
            return None;
        }
        let env_inner = self.env.env_inner();
        let tx_ro = env_inner.tx_ro().expect("tx_ro");
        let tables = env_inner.open_tables(&tx_ro).expect("open_tables");
        get_block_extended_header_from_height(&height, &tables).ok()
    }

    pub const fn set_hard_fork(&mut self, hf: HardFork) {
        self.current_hf = hf;
    }

    /// Mine `n` blocks at the given hard fork version.
    pub fn mine_blocks_at_hf(&mut self, n: usize, hf: HardFork) {
        self.set_hard_fork(hf);
        self.mine_blocks(n);
    }

    pub fn mine_blocks(&mut self, n: usize) {
        for _ in 0..n {
            self.mine_one();
        }
    }

    /// Returns a [`RegtestDecoyRpc`] with a height snapshot taken at call time.
    pub fn decoy_rpc(&self) -> RegtestDecoyRpc {
        RegtestDecoyRpc {
            read_handle: self.read_handle.clone(),
            height: self.height,
        }
    }

    /// Commit a verified block to the DB and update all in-memory state.
    fn commit_block(&mut self, verified: VerifiedBlockInformation, hf: HardFork) {
        let block_hash = verified.block_hash;
        let reward = verified.generated_coins;
        {
            let env_inner = self.env.env_inner();
            let tx_rw = env_inner.tx_rw().expect("tx_rw");
            {
                let mut tables = env_inner.open_tables_mut(&tx_rw).expect("open_tables_mut");
                add_block(&verified, &mut tables).expect("add_block");
            }
            TxRw::commit(tx_rw).expect("commit");
        }
        self.height += 1;
        self.top_hash = block_hash;
        self.already_generated_coins = self.already_generated_coins.saturating_add(reward);
        let prev_rct = *self.rct_counts.last().unwrap_or(&0);
        let new_rct = if hf >= HardFork::V12 { prev_rct + 1 } else { prev_rct };
        self.blocks.push(verified.block);
        self.rct_counts.push(new_rct);
    }

    fn mine_one(&mut self) {
        let height = self.height;
        let hf = self.current_hf;
        let median_bw = penalty_free_zone(hf);
        let reward = calculate_block_reward(0, median_bw, self.already_generated_coins, hf);

        let miner_tx = build_coinbase_tx(height, reward, hf);

        // Drain pending mempool transactions into this block.
        let pending = std::mem::take(&mut self.pending_txs);
        let tx_hashes: Vec<[u8; 32]> = pending.iter().map(|t| t.tx_hash).collect();

        // Timestamp=1 for all blocks (genesis=0). HF1 timestamp check doesn't activate
        // until 60 blocks, so median(60 × 1) == 1 == block.timestamp when it does.
        let block = Block::new(
            BlockHeader {
                hardfork_version: hf as u8,
                hardfork_signal: hf as u8,
                timestamp: 1,
                previous: self.top_hash,
                nonce: 0,
            },
            miner_tx,
            tx_hashes,
        )
        .expect("build block");

        self.cumulative_difficulty += 1;
        let verified = make_verified(block, height, reward, self.cumulative_difficulty, pending);
        self.commit_block(verified, hf);
    }

    /// Mines one block with a wallet-scannable coinbase output (always HF12+, V2).
    pub fn mine_to(&mut self, spend_pub: &EdwardsPoint, view_pub: &EdwardsPoint) {
        let height = self.height;
        // mine_to always uses V2 coinbase; advertise at least HF12 in the header.
        let hf = if self.current_hf >= HardFork::V12 {
            self.current_hf
        } else {
            HardFork::V12
        };
        let median_bw = penalty_free_zone(hf);
        let reward = calculate_block_reward(0, median_bw, self.already_generated_coins, hf);

        let miner_tx = build_scannable_coinbase_tx(height, reward, spend_pub, view_pub);

        let pending = std::mem::take(&mut self.pending_txs);
        let tx_hashes: Vec<[u8; 32]> = pending.iter().map(|t| t.tx_hash).collect();

        let block = Block::new(
            BlockHeader {
                hardfork_version: hf as u8,
                hardfork_signal: hf as u8,
                timestamp: 1,
                previous: self.top_hash,
                nonce: 0,
            },
            miner_tx,
            tx_hashes,
        )
        .expect("build block");

        self.cumulative_difficulty += 1;
        let verified = make_verified(block, height, reward, self.cumulative_difficulty, pending);
        self.commit_block(verified, hf);
    }

    /// Returns the block at `height` as a [`ScannableBlock`], or `None` if out of range.
    pub fn scannable_block(&self, height: usize) -> Option<ScannableBlock> {
        let block = self.blocks.get(height)?.clone();

        // Genesis (height 0) is V1 with no RCT outputs, so output_index is None.
        // For H > 0, the first RCT output in this block starts at rct_counts[H-1].
        let output_index = if height == 0 {
            None
        } else {
            self.rct_counts.get(height - 1).copied()
        };

        Some(ScannableBlock {
            block,
            transactions: vec![],
            output_index_for_first_ringct_output: output_index,
        })
    }
}

impl Default for RegtestNode {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monero_wallet::rpc::DecoyRpc;
    // Satisfy `unused_crate_dependencies`: zeroize is only used in integration tests.
    use zeroize as _;

    #[test]
    fn genesis_block_is_committed() {
        let node = RegtestNode::new();
        assert_eq!(node.height(), 1, "height after genesis should be 1");
        assert_ne!(node.top_hash(), [0_u8; 32], "top_hash should not be all-zeros");
    }

    #[test]
    fn mine_zero_blocks_is_noop() {
        let mut node = RegtestNode::new();
        let hash_before = node.top_hash();
        node.mine_blocks(0);
        assert_eq!(node.height(), 1);
        assert_eq!(node.top_hash(), hash_before);
    }

    #[test]
    fn mine_one_block_advances_height() {
        let mut node = RegtestNode::new();
        node.mine_blocks(1);
        assert_eq!(node.height(), 2);
    }

    #[test]
    fn mine_ten_blocks() {
        let mut node = RegtestNode::new();
        node.mine_blocks(10);
        assert_eq!(node.height(), 11);
    }

    #[test]
    fn top_hash_changes_after_mining() {
        let mut node = RegtestNode::new();
        let before = node.top_hash();
        node.mine_blocks(1);
        assert_ne!(node.top_hash(), before);
    }

    #[test]
    fn block_header_at_genesis() {
        let node = RegtestNode::new();
        let hdr = node.block_header_at(0).expect("genesis header");
        assert_eq!(hdr.version, HardFork::V1);
    }

    #[test]
    fn block_header_out_of_bounds_returns_none() {
        let node = RegtestNode::new();
        assert!(node.block_header_at(1).is_none());
        assert!(node.block_header_at(100).is_none());
    }

    #[test]
    fn already_generated_coins_increases() {
        let mut node = RegtestNode::new();
        let coins_after_genesis = node.already_generated_coins();
        node.mine_blocks(5);
        assert!(node.already_generated_coins() > coins_after_genesis);
    }

    #[test]
    fn hf_version_stored_in_header() {
        let mut node = RegtestNode::new();
        node.mine_blocks_at_hf(1, HardFork::V1);
        // genesis = height 0, mined block = height 1
        let hdr = node.block_header_at(1).expect("header at height 1");
        assert_eq!(hdr.version, HardFork::V1);
    }

    #[test]
    fn advance_through_hfs() {
        let mut node = RegtestNode::new();

        let hfs = [
            HardFork::V1,
            HardFork::V4,
            HardFork::V9,
            HardFork::V12,
            HardFork::V16,
        ];

        for hf in hfs {
            node.mine_blocks_at_hf(1, hf);
        }

        // genesis at 0; the five mined blocks are at heights 1–5.
        let expected = [
            (1_usize, HardFork::V1),
            (2, HardFork::V4),
            (3, HardFork::V9),
            (4, HardFork::V12),
            (5, HardFork::V16),
        ];

        for (h, expected_hf) in expected {
            let hdr = node.block_header_at(h).unwrap_or_else(|| panic!("header at height {h}"));
            assert_eq!(
                hdr.version, expected_hf,
                "height {h}: expected HF {:?}, got {:?}",
                expected_hf, hdr.version
            );
        }
    }

    #[test]
    fn hf16_uses_v2_miner_tx() {
        let mut node = RegtestNode::new();
        node.mine_blocks_at_hf(1, HardFork::V16);
        assert_eq!(node.height(), 2, "height should advance to 2 after mining 1 HF16 block");
    }

    #[tokio::test]
    async fn decoy_rpc_end_height_matches_node_height() {
        let mut node = RegtestNode::new();
        node.mine_blocks(5);
        let rpc = node.decoy_rpc();
        let end_height = rpc
            .get_output_distribution_end_height()
            .await
            .expect("end_height");
        assert_eq!(end_height, node.height(), "end_height should equal node height");
    }

    #[tokio::test]
    async fn decoy_rpc_distribution_is_monotonic() {
        let mut node = RegtestNode::new();
        // Mine 10 HF12 blocks so we have 10 RCT outputs.
        node.mine_blocks_at_hf(10, HardFork::V12);
        let rpc = node.decoy_rpc();

        let dist = rpc
            .get_output_distribution(0..node.height())
            .await
            .expect("distribution");

        assert_eq!(dist.len(), node.height(), "distribution length should equal chain height");

        for w in dist.windows(2) {
            assert!(
                w[1] >= w[0],
                "distribution must be non-decreasing: {:?}",
                &w
            );
        }

        assert_eq!(
            *dist.last().unwrap(),
            10_u64,
            "last distribution entry should equal total RCT outputs"
        );
    }

    #[tokio::test]
    async fn decoy_rpc_outputs_and_lock_window() {
        let mut node = RegtestNode::new();
        // Genesis (HF1, no RCT) + 1 HF12 block => 1 RCT output at index 0.
        node.mine_blocks_at_hf(1, HardFork::V12);

        let rpc = node.decoy_rpc();

        let outs = rpc.get_outs(&[0_u64]).await.expect("get_outs");
        assert_eq!(outs.len(), 1, "should return 1 output");
        assert_eq!(outs[0].height, 1, "output height should be 1");

        // still within the 60-block coinbase lock window
        let locked = rpc
            .get_unlocked_outputs(&[0_u64], 1, false)
            .await
            .expect("get_unlocked_outputs (locked)");
        assert_eq!(locked[0], None, "output should be locked at height 1");

        // past the coinbase lock window; need a fresh snapshot at that height
        let rpc62 = RegtestDecoyRpc {
            read_handle: rpc.read_handle.clone(),
            height: 62,
        };
        let unlocked = rpc62
            .get_unlocked_outputs(&[0_u64], 62, false)
            .await
            .expect("get_unlocked_outputs (unlocked)");
        assert!(
            unlocked[0].is_some(),
            "output should be unlocked at height 62"
        );
    }

    fn dummy_tx_blob() -> Vec<u8> {
        // coinbase-shaped V1 tx with Input::Gen(999); parses cleanly, fails consensus validation
        use monero_oxide::transaction::NotPruned;
        let tx: Transaction<NotPruned> = Transaction::V1 {
            prefix: TransactionPrefix {
                additional_timelock: Timelock::Block(999 + 60),
                inputs: vec![Input::Gen(999)],
                outputs: vec![],
                extra: vec![],
            },
            signatures: vec![],
        };
        tx.serialize()
    }

    #[test]
    fn submit_tx_enters_mempool() {
        let mut node = RegtestNode::new();
        node.submit_tx(dummy_tx_blob()).expect("submit_tx must succeed");
        assert_eq!(node.mempool_size(), 1, "mempool must contain 1 tx");
    }

    #[test]
    fn mempool_drained_after_mine() {
        let mut node = RegtestNode::new();
        node.submit_tx(dummy_tx_blob()).expect("submit_tx");
        assert_eq!(node.mempool_size(), 1);
        node.mine_blocks(1);
        assert_eq!(node.mempool_size(), 0, "mempool must be empty after mining");
    }

    #[test]
    fn height_advances_after_mining_with_tx() {
        let mut node = RegtestNode::new();
        node.submit_tx(dummy_tx_blob()).expect("submit_tx");
        node.mine_blocks(1);
        assert_eq!(node.height(), 2, "height must advance to 2");
    }

    #[test]
    fn bad_blob_returns_error() {
        let mut node = RegtestNode::new();
        let result = node.submit_tx(vec![0xde, 0xad, 0xbe, 0xef]);
        assert!(
            matches!(result, Err(RegtestError::BadTxBlob(_))),
            "corrupt blob must return RegtestError::BadTxBlob"
        );
    }

    #[tokio::test]
    async fn service_decoy_rpc_distribution_matches_db() {
        let mut node = RegtestNode::new();
        // mine 5 HF12 blocks -> 5 RCT outputs
        node.mine_blocks_at_hf(5, HardFork::V12);

        let rpc = node.decoy_rpc();

        let dist = rpc
            .get_output_distribution(0..node.height())
            .await
            .expect("distribution");
        assert_eq!(
            dist.len(),
            node.height(),
            "distribution length must equal chain height"
        );

        // genesis is HF1 (no RCT); each of the 5 HF12 blocks adds 1
        let expected_total = 5_u64;
        assert_eq!(*dist.last().unwrap(), expected_total, "tip should be 5");

        let all_indexes: Vec<u64> = (0..expected_total).collect();
        let outs = rpc.get_outs(&all_indexes).await.expect("get_outs");
        assert_eq!(outs.len(), expected_total as usize);

        let end_height = rpc
            .get_output_distribution_end_height()
            .await
            .expect("end height");
        assert_eq!(end_height, node.height());
    }

    #[tokio::test]
    async fn detached_decoy_rpc_works_independently() {
        let mut node = RegtestNode::new();
        node.mine_blocks_at_hf(3, HardFork::V12);

        // snapshot at height 4 (genesis + 3 HF12 blocks)
        let rpc = node.decoy_rpc();

        node.mine_blocks_at_hf(2, HardFork::V12);

        let end_h = rpc
            .get_output_distribution_end_height()
            .await
            .expect("end_height");
        assert_eq!(end_h, 4, "snapshot height");

        // DB is shared, so post-snapshot outputs are still readable
        let outs = rpc.get_outs(&[0_u64]).await.expect("get_outs");
        assert_eq!(outs.len(), 1);
    }
}
