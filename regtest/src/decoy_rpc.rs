//! [`DecoyRpc`] impl backed by the blockchain read service.

use std::{num::NonZero, ops::RangeBounds};

use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint as DalekEdwardsPoint};

use monero_oxide::COINBASE_LOCK_WINDOW;
use monero_wallet::rpc::{DecoyRpc, OutputInformation, RpcError};

use tower::{Service, ServiceExt};

use cuprate_blockchain::service::BlockchainReadHandle;
use cuprate_types::{
    blockchain::{BlockchainReadRequest, BlockchainResponse},
    OutputDistributionInput,
};

/// [`DecoyRpc`] backed by the blockchain read service.
#[derive(Clone)]
pub struct RegtestDecoyRpc {
    pub(crate) read_handle: BlockchainReadHandle,
    /// Chain height snapshot from when this was constructed.
    pub(crate) height: usize,
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
        let mut read_handle = self.read_handle.clone();
        let chain_height = self.height;
        async move {
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

            // Block 0 is always V1 (no RCT outputs). NonZero cannot represent
            // to_height=0, but that edge case only requests the genesis which
            // has a cumulative RCT count of 0.
            if to == 0 {
                return Ok(vec![0_u64; to - from + 1]);
            }

            let input = OutputDistributionInput {
                amounts: vec![0],
                cumulative: true,
                from_height: from as u64,
                to_height: NonZero::new(to as u64),
            };

            let BlockchainResponse::OutputDistribution(mut data) = read_handle
                .ready()
                .await
                .map_err(|e| RpcError::InternalError(e.to_string()))?
                .call(BlockchainReadRequest::OutputDistribution(input))
                .await
                .map_err(|e| RpcError::InternalError(e.to_string()))?
            else {
                return Err(RpcError::InternalError("unexpected response".into()));
            };

            if data.is_empty() {
                return Err(RpcError::InternalError(
                    "empty distribution response".into(),
                ));
            }

            Ok(data.remove(0).distribution)
        }
    }

    fn get_outs(
        &self,
        indexes: &[u64],
    ) -> impl Send + std::future::Future<Output = Result<Vec<OutputInformation>, RpcError>> {
        let mut read_handle = self.read_handle.clone();
        let outputs_req: Vec<(u64, u64)> = indexes.iter().map(|&idx| (0_u64, idx)).collect();
        async move {
            let BlockchainResponse::OutputsVec(grouped) = read_handle
                .ready()
                .await
                .map_err(|e| RpcError::InternalError(e.to_string()))?
                .call(BlockchainReadRequest::OutputsVec {
                    outputs: outputs_req,
                    get_txid: false,
                })
                .await
                .map_err(|e| RpcError::InternalError(e.to_string()))?
            else {
                return Err(RpcError::InternalError("unexpected response".into()));
            };

            // amount=0 → single group.
            let mut result = Vec::new();
            for (_amount, outputs_list) in grouped {
                for (_amount_index, on_chain) in outputs_list {
                    let key = CompressedEdwardsY(on_chain.key.0);
                    let commitment = CompressedEdwardsY(on_chain.commitment.0)
                        .decompress()
                        .ok_or_else(|| {
                            RpcError::InvalidNode("invalid commitment point".into())
                        })?;
                    result.push(OutputInformation {
                        height: on_chain.height,
                        // We do our own lock check in get_unlocked_outputs; report unlocked=true here.
                        unlocked: true,
                        key,
                        commitment,
                        transaction: on_chain.txid.unwrap_or([0_u8; 32]),
                    });
                }
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
