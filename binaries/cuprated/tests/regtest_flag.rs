#![allow(unused_crate_dependencies)]

#[cfg(feature = "regtest")]
#[test]
fn regtest_height_after_3_blocks() {
    let mut node = cuprate_regtest::RegtestNode::new();
    node.mine_blocks(3);
    assert_eq!(node.height(), 4);
}
