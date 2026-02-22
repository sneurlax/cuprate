//! Smoke-test entry point for `--regtest`.
//!
//! Mines 10 blocks into a temporary database and prints the height and top
//! hash. Intended for CI.

pub fn run_regtest() {
    let mut node = cuprate_regtest::RegtestNode::new();
    node.mine_blocks(10);
    let height = node.height();
    let top_hash = node.top_hash();
    let hex: String = top_hash.iter().map(|b| format!("{b:02x}")).collect();
    println!("height={height} top_hash={hex}");
}
