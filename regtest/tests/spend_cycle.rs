#![expect(unused_crate_dependencies, reason = "outer test module")]

use curve25519_dalek::{constants::ED25519_BASEPOINT_TABLE, Scalar};
use monero_oxide::primitives::keccak256_to_scalar;
use monero_wallet::{
    address::{MoneroAddress, Network},
    ringct::RctType,
    rpc::{DecoyRpc, FeeRate},
    send::{Change, SignableTransaction},
    transaction::{NotPruned, Transaction},
    OutputWithDecoys, Scanner, ViewPair, WalletOutput,
};
use rand::rngs::OsRng;
use rand::RngCore;
use zeroize::Zeroizing;

use cuprate_consensus_rules::hard_forks::HardFork;
use cuprate_regtest::{RegtestDecoyRpc, RegtestNode};

async fn build_spend_tx(
    output: WalletOutput,
    spend_scalar: &Zeroizing<Scalar>,
    recipient: &MoneroAddress,
    amount: u64,
    fee_rate: FeeRate,
    decoy_rpc: &RegtestDecoyRpc,
) -> Transaction<NotPruned> {
    let rct_type = RctType::ClsagBulletproofPlus;
    let ring_len: u8 = 16;

    let height = decoy_rpc
        .get_output_distribution_end_height()
        .await
        .expect("get_output_distribution_end_height");

    let output_with_decoys = OutputWithDecoys::fingerprintable_deterministic_new(
        &mut OsRng,
        decoy_rpc,
        ring_len,
        height,
        output,
    )
    .await
    .expect("fingerprintable_deterministic_new");

    let spend_pub = &**spend_scalar * ED25519_BASEPOINT_TABLE;
    let view_scalar = Zeroizing::new(keccak256_to_scalar(spend_scalar.as_bytes()));
    let change_view_pair =
        ViewPair::new(spend_pub, view_scalar).expect("change ViewPair");
    let change = Change::new(change_view_pair, None);

    let mut outgoing_view_key = Zeroizing::new([0_u8; 32]);
    OsRng.fill_bytes(outgoing_view_key.as_mut());

    let signable = SignableTransaction::new(
        rct_type,
        outgoing_view_key,
        vec![output_with_decoys],
        vec![(*recipient, amount)],
        change,
        vec![],
        fee_rate,
    )
    .expect("SignableTransaction::new");

    signable.sign(&mut OsRng, spend_scalar).expect("sign")
}

#[tokio::test]
async fn spend_cycle_mine_scan_mature_spend_mine() {
    let spend_scalar = Zeroizing::new(Scalar::random(&mut OsRng));
    let spend_pub = &*spend_scalar * ED25519_BASEPOINT_TABLE;
    let view_scalar = Zeroizing::new(keccak256_to_scalar(spend_scalar.as_bytes()));
    let view_pub = &*view_scalar * ED25519_BASEPOINT_TABLE;

    let view_pair =
        ViewPair::new(spend_pub, view_scalar.clone()).expect("ViewPair");
    let mut scanner = Scanner::new(view_pair);

    let mut node = RegtestNode::new();
    node.set_hard_fork(HardFork::V16);
    node.mine_to(&spend_pub, &view_pub);
    // output at height 1 needs 60 more to clear the coinbase lock
    node.mine_blocks(60);
    assert_eq!(node.height(), 62);

    let scannable = node.scannable_block(1).expect("scannable_block(1)");
    let timelocked = scanner.scan(scannable).expect("scan");
    let outputs = timelocked.ignore_additional_timelock();
    assert_eq!(outputs.len(), 1);
    assert!(outputs[0].commitment().amount > 0);

    let output = outputs[0].clone();
    let amount = output.commitment().amount / 2;
    let recipient = ViewPair::new(spend_pub, view_scalar.clone())
        .expect("recipient ViewPair")
        .legacy_address(Network::Mainnet);
    let fee_rate = FeeRate::new(20_000, 10_000).expect("FeeRate");
    let decoy_rpc = node.decoy_rpc();

    let tx =
        build_spend_tx(output, &spend_scalar, &recipient, amount, fee_rate, &decoy_rpc).await;

    let tx_hash = node.submit_tx(tx.serialize()).expect("submit_tx");
    assert_ne!(tx_hash, [0_u8; 32]);

    node.mine_blocks(1);
    assert_eq!(node.height(), 63);
}
