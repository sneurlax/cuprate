#![allow(clippy::significant_drop_tightening)]
//! [`DecoyRpc`] impl backed directly by the regtest heed DB.
//!
//! Accesses [`ConcreteEnv`] synchronously inside the `async move` bodies so
//! we don't need a separate blockchain read service. HF12+ blocks each
//! contribute one RCT coinbase output; earlier blocks contribute none.
//! The cumulative distribution is read from the [`RctOutputs`] table that
//! `add_block` keeps up to date.

use std::ops::RangeBounds;
use std::sync::Arc;

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint as DalekEdwardsPoint};

use monero_oxide::COINBASE_LOCK_WINDOW;
use monero_wallet::rpc::{DecoyRpc, OutputInformation, RpcError};

use cuprate_blockchain::{
    ops::output::{get_rct_num_outputs, get_rct_output},
    tables::{OpenTables, Tables},
};
use cuprate_database::{ConcreteEnv, Env, EnvInner};

/// [`DecoyRpc`] backed by the regtest DB. Obtain via [`crate::RegtestNode::decoy_rpc`].
#[derive(Clone)]
pub struct RegtestDecoyRpc {
    pub(crate) env: Arc<ConcreteEnv>,
    /// Chain height snapshot from when this was constructed.
    pub(crate) height: usize,
}

impl RegtestDecoyRpc {
    fn total_rct_outputs_sync(env: &ConcreteEnv) -> Result<u64, RpcError> {
        let env_inner = env.env_inner();
        let tx_ro = env_inner
            .tx_ro()
            .map_err(|e| RpcError::InternalError(e.to_string()))?;
        let tables = env_inner
            .open_tables(&tx_ro)
            .map_err(|e| RpcError::InternalError(e.to_string()))?;
        get_rct_num_outputs(tables.rct_outputs())
            .map_err(|e| RpcError::InternalError(e.to_string()))
    }

    fn get_rct_output_sync(
        env: &ConcreteEnv,
        index: u64,
    ) -> Result<OutputInformation, RpcError> {
        let env_inner = env.env_inner();
        let tx_ro = env_inner
            .tx_ro()
            .map_err(|e| RpcError::InternalError(e.to_string()))?;
        let tables = env_inner
            .open_tables(&tx_ro)
            .map_err(|e| RpcError::InternalError(e.to_string()))?;

        let rct_out = get_rct_output(&index, tables.rct_outputs())
            .map_err(|e| RpcError::InternalError(format!("rct output {index}: {e}")))?;

        let key = CompressedEdwardsY(rct_out.key);
        let commitment = CompressedEdwardsY(rct_out.commitment)
            .decompress()
            .ok_or_else(|| {
                RpcError::InvalidNode(format!("rct output {index} has invalid commitment point"))
            })?;

        Ok(OutputInformation {
            height: rct_out.height as usize,
            // We do our own lock check in get_unlocked_outputs; report unlocked=true here.
            unlocked: true,
            key,
            commitment,
            // Regtest coinbase outputs have no separate tx blob; use zero txid.
            transaction: [0_u8; 32],
        })
    }
}

impl DecoyRpc for RegtestDecoyRpc {
    fn get_output_distribution_end_height(
        &self,
    ) -> impl Send + std::future::Future<Output = Result<usize, RpcError>> {
        let height = self.height;
        async move { Ok(height) }
    }

    fn get_output_distribution(
        &self,
        range: impl Send + RangeBounds<usize>,
    ) -> impl Send + std::future::Future<Output = Result<Vec<u64>, RpcError>> {
        let env = Arc::clone(&self.env);
        let chain_height = self.height;
        async move {
            let total_rct = Self::total_rct_outputs_sync(&env)?;

            // Resolve inclusive bounds.
            let from = match range.start_bound() {
                std::ops::Bound::Included(&f) => f,
                std::ops::Bound::Excluded(&f) => f.saturating_add(1),
                std::ops::Bound::Unbounded => 0,
            };
            let to = match range.end_bound() {
                std::ops::Bound::Included(&t) => t,
                std::ops::Bound::Excluded(&t) => t.saturating_sub(1),
                std::ops::Bound::Unbounded => chain_height.saturating_sub(1),
            };

            if from > to {
                return Err(RpcError::InternalError(format!(
                    "empty range from={from} to={to}"
                )));
            }

            // The first block that contributes an RCT output starts at offset
            // `rct_start = chain_height - total_rct` (0-indexed).
            let rct_start = chain_height.saturating_sub(usize::try_from(total_rct).unwrap_or(usize::MAX));

            let dist: Vec<u64> = (from..=to)
                .map(|h| {
                    if h < rct_start {
                        0_u64
                    } else {
                        // Number of HF12+ blocks from rct_start up to and including h.
                        (h - rct_start + 1) as u64
                    }
                })
                .collect();

            Ok(dist)
        }
    }

    fn get_outs(
        &self,
        indexes: &[u64],
    ) -> impl Send + std::future::Future<Output = Result<Vec<OutputInformation>, RpcError>> {
        let env = Arc::clone(&self.env);
        let indexes = indexes.to_vec();
        async move {
            let mut result = Vec::with_capacity(indexes.len());
            for idx in &indexes {
                result.push(Self::get_rct_output_sync(&env, *idx)?);
            }
            Ok(result)
        }
    }

    fn get_unlocked_outputs(
        &self,
        indexes: &[u64],
        height: usize,
        _fingerprintable_deterministic: bool,
    ) -> impl Send
           + std::future::Future<Output = Result<Vec<Option<[DalekEdwardsPoint; 2]>>, RpcError>>
    {
        let decoy_rpc = self.clone();
        let indexes = indexes.to_vec();
        async move {
            let outs = decoy_rpc.get_outs(&indexes).await?;

            let result = outs
                .into_iter()
                .map(|out| {
                    // All regtest outputs are coinbase, so apply 60-block lock.
                    let locked_until = out.height + COINBASE_LOCK_WINDOW;
                    if locked_until > height {
                        return Ok(None);
                    }

                    let Some(key) = out.key.decompress() else {
                        return Ok(None);
                    };
                    Ok(Some([key, out.commitment]))
                })
                .collect::<Result<Vec<_>, RpcError>>()?;

            Ok(result)
        }
    }
}
