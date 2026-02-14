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
//! All blocks use V1 transactions. PoW hash is always `[0u8; 32]`; difficulty
//! is always 1. The default coinbase output key is a fixed well-known point.

use tempfile::TempDir;

use monero_oxide::{
    block::{Block, BlockHeader},
    io::CompressedPoint,
    transaction::{Input, Output, Timelock, Transaction, TransactionPrefix},
};

use cuprate_blockchain::{
    config::ConfigBuilder,
    ops::block::{add_block, get_block_extended_header_from_height},
    tables::OpenTables,
};
use cuprate_consensus_rules::{
    blocks::PENALTY_FREE_ZONE_1,
    hard_forks::HardFork,
    miner_tx::calculate_block_reward,
};
use cuprate_database::{ConcreteEnv, Env, EnvInner, TxRw};
use cuprate_types::{ExtendedBlockHeader, VerifiedBlockInformation};

/// Placeholder output key for non-scanned coinbase outputs (regtest only).
///
/// A fixed, publicly-known point (the Ed25519 basepoint G) used as a dummy output key.
const REGTEST_MINER_KEY: CompressedPoint = CompressedPoint([
    0xe4, 0xa7, 0x38, 0x4d, 0xaf, 0xea, 0xc8, 0x85,
    0xc0, 0x6c, 0x5e, 0x1a, 0x3a, 0x05, 0x16, 0x72,
    0xf6, 0xf6, 0x5f, 0x90, 0x0a, 0x1e, 0x85, 0x01,
    0x38, 0x95, 0xd1, 0x72, 0x24, 0xa7, 0x1b, 0x5b,
]);

/// V1 coinbase for `chain_height` claiming `reward` atoms.
fn build_coinbase_tx(chain_height: usize, reward: u64) -> Transaction {
    Transaction::V1 {
        prefix: TransactionPrefix {
            additional_timelock: Timelock::Block(chain_height + 60),
            inputs: vec![Input::Gen(chain_height)],
            outputs: vec![Output {
                amount: Some(reward),
                key: REGTEST_MINER_KEY,
                view_tag: None,
            }],
            extra: vec![],
        },
        signatures: vec![],
    }
}

/// Wrap a block in [`VerifiedBlockInformation`] with dummy PoW and difficulty 1.
fn make_verified(
    block: Block,
    height: usize,
    reward: u64,
    cumulative_difficulty: u128,
) -> VerifiedBlockInformation {
    let block_blob = block.serialize();
    let block_hash = block.hash();
    let weight = block_blob.len();
    VerifiedBlockInformation {
        block,
        block_blob,
        txs: vec![],
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
/// then [`mine_blocks`](RegtestNode::mine_blocks) to grow the chain.
/// Drop cleans up the temp directory automatically.
pub struct RegtestNode {
    env: ConcreteEnv,
    _tmp: TempDir,
    /// Next block height to be mined (= number of blocks committed so far).
    height: usize,
    /// Hash of the most-recently-committed block.
    top_hash: [u8; 32],
    /// Total coins minted in all committed blocks.
    already_generated_coins: u64,
    /// Cumulative difficulty of the committed chain (increases by 1 per block).
    cumulative_difficulty: u128,
}

impl RegtestNode {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = ConfigBuilder::new()
            .data_directory(tmp.path().to_owned())
            .build();
        let env = cuprate_blockchain::open(config).expect("open blockchain db");

        let hf = HardFork::V1;
        let genesis_reward = calculate_block_reward(0, PENALTY_FREE_ZONE_1, 0, hf);
        let miner_tx = build_coinbase_tx(0, genesis_reward);

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
        let verified = make_verified(genesis, 0, genesis_reward, 1);

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
            _tmp: tmp,
            height: 1,
            top_hash: genesis_hash,
            already_generated_coins: genesis_reward,
            cumulative_difficulty: 1,
        }
    }

    /// Number of blocks committed (genesis counts as 1).
    pub fn height(&self) -> usize {
        self.height
    }

    /// Hash of the most-recently-committed block.
    pub fn top_hash(&self) -> [u8; 32] {
        self.top_hash
    }

    /// Total coins minted so far.
    pub fn already_generated_coins(&self) -> u64 {
        self.already_generated_coins
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

    pub fn mine_blocks(&mut self, n: usize) {
        for _ in 0..n {
            self.mine_one();
        }
    }

    fn mine_one(&mut self) {
        let height = self.height;
        let hf = HardFork::V1;
        let reward =
            calculate_block_reward(0, PENALTY_FREE_ZONE_1, self.already_generated_coins, hf);

        let miner_tx = build_coinbase_tx(height, reward);

        // All regtest blocks share timestamp = 1 (genesis is 0).
        // For HF1 the timestamp check only activates after 60 blocks; since
        // median of 60 identical timestamps is 1 and block.timestamp == 1,
        // the check passes once consensus validation is eventually enabled.
        let block = Block::new(
            BlockHeader {
                hardfork_version: 1,
                hardfork_signal: 0,
                timestamp: 1,
                previous: self.top_hash,
                nonce: 0,
            },
            miner_tx,
            vec![],
        )
        .expect("build block");

        self.cumulative_difficulty += 1;
        let block_hash = block.hash();
        let verified = make_verified(
            block,
            height,
            reward,
            self.cumulative_difficulty,
            self.already_generated_coins,
        );

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
        self.already_generated_coins =
            self.already_generated_coins.saturating_add(reward);
    }
}

impl Default for RegtestNode {
    fn default() -> Self {
        Self::new()
    }
}
