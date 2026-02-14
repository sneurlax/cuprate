#![expect(unused_crate_dependencies, reason = "outer test module")]
//! Integration tests for `cuprate-regtest`.
//!
//! Each test creates an independent `RegtestNode` backed by its own tempdir.

use cuprate_regtest::RegtestNode;

#[test]
fn genesis_committed_at_init() {
    let node = RegtestNode::new();
    assert_eq!(node.height(), 1, "genesis = height 0, next height = 1");
}

#[test]
fn mine_blocks_advances_height() {
    let mut node = RegtestNode::new();
    node.mine_blocks(10);
    assert_eq!(node.height(), 11);
    node.mine_blocks(5);
    assert_eq!(node.height(), 16);
}

#[test]
fn top_hash_changes_after_each_block() {
    let mut node = RegtestNode::new();
    let h0 = node.top_hash();
    node.mine_blocks(1);
    let h1 = node.top_hash();
    node.mine_blocks(1);
    let h2 = node.top_hash();

    assert_ne!(h0, h1, "hash must change after block 1");
    assert_ne!(h1, h2, "hash must change after block 2");
    assert_ne!(h0, h2, "all three hashes must be distinct");
}

#[test]
fn block_header_at_returns_correct_data() {
    let mut node = RegtestNode::new();
    node.mine_blocks(4);
    assert_eq!(node.height(), 5);

    for h in 0..5 {
        assert!(
            node.block_header_at(h).is_some(),
            "height {h} must have a header"
        );
    }

    assert!(
        node.block_header_at(5).is_none(),
        "height 5 not yet mined"
    );
}

#[test]
fn cumulative_difficulty_increases_monotonically() {
    let mut node = RegtestNode::new();
    let h0 = node.block_header_at(0).unwrap();
    assert_eq!(h0.cumulative_difficulty, 1);

    node.mine_blocks(3);

    let h1 = node.block_header_at(1).unwrap();
    let h2 = node.block_header_at(2).unwrap();
    let h3 = node.block_header_at(3).unwrap();
    assert_eq!(h1.cumulative_difficulty, 2);
    assert_eq!(h2.cumulative_difficulty, 3);
    assert_eq!(h3.cumulative_difficulty, 4);
}

#[test]
fn generated_coins_accumulate() {
    let mut node = RegtestNode::new();
    let coins_after_genesis = node.already_generated_coins();
    assert!(
        coins_after_genesis > 0,
        "genesis reward must be non-zero (got {coins_after_genesis})"
    );

    node.mine_blocks(5);
    let coins_after_5 = node.already_generated_coins();
    assert!(
        coins_after_5 > coins_after_genesis,
        "coins must grow as blocks are mined"
    );
}

#[test]
fn default_matches_new() {
    let a = RegtestNode::new();
    let b = RegtestNode::default();
    assert_eq!(a.height(), b.height());
    assert_eq!(
        a.already_generated_coins(),
        b.already_generated_coins()
    );
    assert_eq!(
        a.top_hash(),
        b.top_hash(),
        "genesis hash must be deterministic"
    );
}

#[test]
fn mine_100_blocks_without_error() {
    let mut node = RegtestNode::new();
    node.mine_blocks(100);
    assert_eq!(node.height(), 101);
    assert!(node.already_generated_coins() > 0);
}
