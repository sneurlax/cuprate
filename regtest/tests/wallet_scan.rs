#![expect(unused_crate_dependencies, reason = "outer test module")]
//! Integration test: wallet scanning against a live regtest chain.
//!
//! Demonstrates that `RegtestNode::mine_to` + `RegtestNode::scannable_block`
//! let a `monero_wallet::Scanner` find coinbase outputs in-process.

use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, Scalar};
use monero_wallet::{Scanner, ViewPair};
use rand::rngs::OsRng;
use zeroize::Zeroizing;

use cuprate_regtest::RegtestNode;
use cuprate_consensus_rules::hard_forks::HardFork;
use monero_oxide::primitives::keccak256_to_scalar;

#[test]
fn scanner_detects_coinbase_output_mined_to_wallet() {
    let spend_scalar = Scalar::random(&mut OsRng);
    let spend_pub    = &spend_scalar * ED25519_BASEPOINT_TABLE;
    let view_scalar  = keccak256_to_scalar(spend_scalar.as_bytes());
    let view_pub     = &view_scalar  * ED25519_BASEPOINT_TABLE;

    let view_pair = ViewPair::new(spend_pub, Zeroizing::new(view_scalar))
        .expect("valid view pair");
    let mut scanner = Scanner::new(view_pair);

    let mut node = RegtestNode::new();
    node.set_hard_fork(HardFork::V16);
    node.mine_to(&spend_pub, &view_pub);

    node.mine_blocks(60);
    assert_eq!(node.height(), 62);

    let scannable = node.scannable_block(1).expect("block 1 must exist");
    let timelocked = scanner.scan(scannable).expect("scan must succeed");
    let outputs = timelocked.ignore_additional_timelock();

    assert_eq!(outputs.len(), 1, "scanner must detect exactly one coinbase output");
    assert!(
        outputs[0].commitment().amount > 0,
        "coinbase output must carry a non-zero reward"
    );
}

#[test]
fn scanner_finds_nothing_in_foreign_block() {
    let spend_scalar = Scalar::random(&mut OsRng);
    let spend_pub    = &spend_scalar * ED25519_BASEPOINT_TABLE;
    let view_scalar  = keccak256_to_scalar(spend_scalar.as_bytes());

    let view_pair = ViewPair::new(spend_pub, Zeroizing::new(view_scalar))
        .expect("valid view pair");
    let mut scanner = Scanner::new(view_pair);

    let mut node = RegtestNode::new();
    node.set_hard_fork(HardFork::V16);
    node.mine_blocks(1); // goes to REGTEST_MINER_KEY, so the scanner finds nothing

    let scannable = node.scannable_block(1).expect("block 1 must exist");
    let timelocked = scanner.scan(scannable).expect("scan must succeed");
    let outputs = timelocked.ignore_additional_timelock();

    assert_eq!(outputs.len(), 0, "foreign block must yield zero outputs");
}
