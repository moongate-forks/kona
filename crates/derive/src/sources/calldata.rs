//! CallData Source

use crate::{
    errors::{PipelineError, PipelineResult},
    traits::{AsyncIterator, ChainProvider},
};
use alloc::{boxed::Box, collections::VecDeque, format};
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_primitives::{Address, Bytes, TxKind};
use async_trait::async_trait;
use op_alloy_protocol::BlockInfo;

/// A data iterator that reads from calldata.
#[derive(Debug, Clone)]
pub struct CalldataSource<CP>
where
    CP: ChainProvider + Send,
{
    /// The chain provider to use for the calldata source.
    chain_provider: CP,
    /// The batch inbox address.
    batch_inbox_address: Address,
    /// Block Ref
    block_ref: BlockInfo,
    /// The L1 Signer.
    signer: Address,
    /// Current calldata.
    calldata: VecDeque<Bytes>,
    /// Whether the calldata source is open.
    open: bool,
}

impl<CP: ChainProvider + Send> CalldataSource<CP> {
    /// Creates a new calldata source.
    pub fn new(
        chain_provider: CP,
        batch_inbox_address: Address,
        block_ref: BlockInfo,
        signer: Address,
    ) -> Self {
        Self {
            chain_provider,
            batch_inbox_address,
            block_ref,
            signer,
            calldata: VecDeque::new(),
            open: false,
        }
    }

    /// Loads the calldata into the source if it is not open.
    async fn load_calldata(&mut self) -> Result<(), CP::Error> {
        if self.open {
            return Ok(());
        }

        let (_, txs) =
            self.chain_provider.block_info_and_transactions_by_hash(self.block_ref.hash).await?;

        self.calldata = txs
            .iter()
            .filter_map(|tx| {
                let (tx_kind, data) = match tx {
                    TxEnvelope::Legacy(tx) => (tx.tx().to(), tx.tx().input()),
                    TxEnvelope::Eip2930(tx) => (tx.tx().to(), tx.tx().input()),
                    TxEnvelope::Eip1559(tx) => (tx.tx().to(), tx.tx().input()),
                    _ => return None,
                };
                let TxKind::Call(to) = tx_kind else { return None };

                if to != self.batch_inbox_address {
                    return None;
                }
                if tx.recover_signer().ok()? != self.signer {
                    return None;
                }
                Some(data.to_vec().into())
            })
            .collect::<VecDeque<_>>();

        self.open = true;

        Ok(())
    }
}

#[async_trait]
impl<CP: ChainProvider + Send> AsyncIterator for CalldataSource<CP> {
    type Item = Bytes;

    async fn next(&mut self) -> PipelineResult<Self::Item> {
        if self.load_calldata().await.is_err() {
            return Err(PipelineError::Provider(format!(
                "Failed to load calldata for block {}",
                self.block_ref.hash
            ))
            .temp());
        }
        self.calldata.pop_front().ok_or(PipelineError::Eof.temp())
    }
}
