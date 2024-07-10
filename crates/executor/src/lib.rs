#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
// #![no_std]

extern crate alloc;

use alloc::vec::Vec;
use alloy_consensus::{Header, Sealable, EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{address, keccak256, Address, Bytes, TxKind, B256, U256};
use anyhow::{anyhow, Result};
use kona_derive::types::{L2PayloadAttributes, RawTransaction, RollupConfig};
use kona_mpt::{ordered_trie_with_encoder, TrieDB, TrieDBFetcher, TrieDBHinter};
use op_alloy_consensus::{OpReceiptEnvelope, OpTxEnvelope};
use revm::{
    db::{states::bundle_state::BundleRetention, State},
    primitives::{
        calc_excess_blob_gas, BlobExcessGasAndPrice, BlockEnv, CfgEnv, CfgEnvWithHandlerCfg,
        EnvWithHandlerCfg, OptimismFields, SpecId, TransactTo, TxEnv,
    },
    Evm,
};
use tracing::{debug, info};

mod builder;
pub use builder::StatelessL2BlockExecutorBuilder;

mod precompile;
pub use precompile::{NoPrecompileOverride, PrecompileOverride};

mod eip4788;
use eip4788::pre_block_beacon_root_contract_call;

mod canyon;
use canyon::ensure_create2_deployer_canyon;

mod util;
use util::{extract_tx_gas_limit, is_system_transaction, logs_bloom, receipt_envelope_from_parts};

/// The block executor for the L2 client program. Operates off of a [TrieDB] backed [State],
/// allowing for stateless block execution of OP Stack blocks.
#[derive(Debug)]
pub struct StatelessL2BlockExecutor<'a, F, H, PO>
where
    F: TrieDBFetcher,
    H: TrieDBHinter,
    PO: PrecompileOverride<F, H>,
{
    /// The [RollupConfig].
    config: &'a RollupConfig,
    /// The inner state database component.
    state: State<TrieDB<F, H>>,
    /// Phantom data for the precompile overrides.
    _phantom: core::marker::PhantomData<PO>,
}

impl<'a, F, H, PO> StatelessL2BlockExecutor<'a, F, H, PO>
where
    F: TrieDBFetcher,
    H: TrieDBHinter,
    PO: PrecompileOverride<F, H>,
{
    /// Constructs a new [StatelessL2BlockExecutorBuilder] with the given [RollupConfig].
    pub fn builder(config: &'a RollupConfig) -> StatelessL2BlockExecutorBuilder<'a, F, H, PO> {
        StatelessL2BlockExecutorBuilder::with_config(config)
    }

    /// Returns a reference to the current [State] database of the executor.
    pub fn state_ref(&self) -> &State<TrieDB<F, H>> {
        &self.state
    }

    /// Executes the given block, returning the resulting state root.
    ///
    /// ## Steps
    /// 1. Prepare the block environment.
    /// 2. Apply the pre-block EIP-4788 contract call.
    /// 3. Prepare the EVM with the given L2 execution payload in the block environment.
    ///     - Reject any EIP-4844 transactions, as they are not supported on the OP Stack.
    ///     - If the transaction is a deposit, cache the depositor account prior to execution.
    ///     - Construct the EVM with the given configuration.
    ///     - Execute the transaction.
    ///     - Accumulate the gas used by the transaction to the block-scoped cumulative gas used
    ///       counter.
    ///     - Create a receipt envelope for the transaction.
    /// 4. Merge all state transitions into the cache state.
    /// 5. Compute the [state root, transactions root, receipts root, logs bloom] for the processed
    ///    block.
    pub fn execute_payload(&mut self, payload: L2PayloadAttributes) -> Result<&Header> {
        // Prepare the `revm` environment.
        let initialized_block_env = Self::prepare_block_env(
            self.revm_spec_id(payload.timestamp),
            self.config,
            self.state.database.parent_block_header(),
            &payload,
        );
        let initialized_cfg = self.evm_cfg_env(payload.timestamp);
        let block_number = initialized_block_env.number.to::<u64>();
        let base_fee = initialized_block_env.basefee.to::<u128>();
        let gas_limit =
            payload.gas_limit.ok_or(anyhow!("Gas limit not provided in payload attributes"))?;

        info!(
            target: "client_executor",
            "Executing block # {block_number} | Gas limit: {gas_limit} | Tx count: {tx_len}",
            block_number = block_number,
            gas_limit = gas_limit,
            tx_len = payload.transactions.len()
        );

        // Apply the pre-block EIP-4788 contract call.
        pre_block_beacon_root_contract_call(
            &mut self.state,
            self.config,
            block_number,
            &initialized_cfg,
            &initialized_block_env,
            &payload,
        )?;

        // Ensure that the create2 contract is deployed upon transition to the Canyon hardfork.
        ensure_create2_deployer_canyon(&mut self.state, self.config, payload.timestamp)?;

        let mut cumulative_gas_used = 0u64;
        let mut receipts: Vec<OpReceiptEnvelope> = Vec::with_capacity(payload.transactions.len());
        let is_regolith = self.config.is_regolith_active(payload.timestamp);

        // Construct the block-scoped EVM with the given configuration.
        // The transaction environment is set within the loop for each transaction.
        let mut evm = Evm::builder()
            .with_db(&mut self.state)
            .with_env_with_handler_cfg(EnvWithHandlerCfg::new_with_cfg_env(
                initialized_cfg.clone(),
                initialized_block_env.clone(),
                Default::default(),
            ))
            .append_handler_register(PO::set_precompiles)
            .build();

        // Execute the transactions in the payload.
        let transactions = payload
            .transactions
            .iter()
            .map(|raw_tx| {
                let tx = OpTxEnvelope::decode_2718(&mut raw_tx.as_ref()).map_err(|e| anyhow!(e))?;
                Ok((tx, raw_tx.as_ref()))
            })
            .collect::<Result<Vec<_>>>()?;
        for (transaction, raw_transaction) in transactions {
            // The sum of the transaction’s gas limit, Tg, and the gas utilized in this block prior,
            // must be no greater than the block’s gasLimit.
            let block_available_gas = (gas_limit - cumulative_gas_used) as u128;
            if extract_tx_gas_limit(&transaction) > block_available_gas &&
                (is_regolith || !is_system_transaction(&transaction))
            {
                anyhow::bail!("Transaction gas limit exceeds block gas limit")
            }

            // Reject any EIP-4844 transactions.
            if matches!(transaction, OpTxEnvelope::Eip4844(_)) {
                anyhow::bail!("EIP-4844 transactions are not supported");
            }

            // Modify the transaction environment with the current transaction.
            evm = evm
                .modify()
                .with_tx_env(Self::prepare_tx_env(&transaction, raw_transaction)?)
                .build();

            // If the transaction is a deposit, cache the depositor account.
            //
            // This only needs to be done post-Regolith, as deposit nonces were not included in
            // Bedrock. In addition, non-deposit transactions do not have deposit
            // nonces.
            let depositor = is_regolith
                .then(|| {
                    if let OpTxEnvelope::Deposit(deposit) = &transaction {
                        evm.db_mut().load_cache_account(deposit.from).ok().cloned()
                    } else {
                        None
                    }
                })
                .flatten();

            // Execute the transaction.
            let tx_hash = keccak256(raw_transaction);
            debug!(
                target: "client_executor",
                "Executing transaction: {tx_hash}",
            );
            let result = evm.transact_commit().map_err(|e| anyhow!("Fatal EVM Error: {e}"))?;
            debug!(
                target: "client_executor",
                "Transaction executed: {tx_hash} | Gas used: {gas_used} | Success: {status}",
                gas_used = result.gas_used(),
                status = result.is_success()
            );

            // Accumulate the gas used by the transaction.
            cumulative_gas_used += result.gas_used();

            // Create receipt envelope.
            let receipt = receipt_envelope_from_parts(
                result.is_success(),
                cumulative_gas_used as u128,
                result.logs(),
                transaction.tx_type(),
                depositor
                    .as_ref()
                    .map(|depositor| depositor.account_info().unwrap_or_default().nonce),
                depositor
                    .is_some()
                    .then(|| self.config.is_canyon_active(payload.timestamp).then_some(1))
                    .flatten(),
            );
            receipts.push(receipt);
        }

        info!(
            target: "client_executor",
            "Transaction execution complete | Cumulative gas used: {cumulative_gas_used}",
            cumulative_gas_used = cumulative_gas_used
        );

        // Drop the EVM to rid the exclusive reference to the database.
        drop(evm);

        // Merge all state transitions into the cache state.
        debug!(target: "client_executor", "Merging state transitions");
        self.state.merge_transitions(BundleRetention::Reverts);

        // Take the bundle state.
        let bundle = self.state.take_bundle();

        // Recompute the header roots.
        let state_root = self.state.database.state_root(&bundle)?;

        let transactions_root = Self::compute_transactions_root(payload.transactions.as_slice());
        let receipts_root = Self::compute_receipts_root(&receipts, self.config, payload.timestamp);
        debug!(
            target: "client_executor",
            "Computed transactions root: {transactions_root} | receipts root: {receipts_root}",
        );

        // The withdrawals root on OP Stack chains, after Canyon activation, is always the empty
        // root hash.
        let withdrawals_root =
            self.config.is_canyon_active(payload.timestamp).then_some(EMPTY_ROOT_HASH);

        // Compute logs bloom filter for the block.
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|receipt| receipt.logs()));

        // Compute Cancun fields, if active.
        let (blob_gas_used, excess_blob_gas) = self
            .config
            .is_ecotone_active(payload.timestamp)
            .then(|| {
                let parent_header = self.state.database.parent_block_header();
                let excess_blob_gas = if self.config.is_ecotone_active(parent_header.timestamp) {
                    let parent_excess_blob_gas = parent_header.excess_blob_gas.unwrap_or_default();
                    let parent_blob_gas_used = parent_header.blob_gas_used.unwrap_or_default();
                    calc_excess_blob_gas(parent_excess_blob_gas as u64, parent_blob_gas_used as u64)
                } else {
                    // For the first post-fork block, both blob gas fields are evaluated to 0.
                    calc_excess_blob_gas(0, 0)
                };

                (Some(0), Some(excess_blob_gas as u128))
            })
            .unwrap_or_default();

        // Construct the new header.
        let header = Header {
            parent_hash: self.state.database.parent_block_header().seal(),
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: payload.fee_recipient,
            state_root,
            transactions_root,
            receipts_root,
            requests_root: None,
            withdrawals_root,
            logs_bloom,
            difficulty: U256::ZERO,
            number: block_number,
            gas_limit: gas_limit.into(),
            gas_used: cumulative_gas_used as u128,
            timestamp: payload.timestamp,
            mix_hash: payload.prev_randao,
            nonce: Default::default(),
            base_fee_per_gas: Some(base_fee),
            blob_gas_used,
            excess_blob_gas,
            parent_beacon_block_root: payload.parent_beacon_block_root,
            // Provide no extra data on OP Stack chains
            extra_data: Bytes::default(),
        }
        .seal_slow();

        info!(
            target: "client_executor",
            "Sealed new header | Hash: {header_hash} | State root: {state_root} | Transactions root: {transactions_root} | Receipts root: {receipts_root}",
            header_hash = header.seal(),
            state_root = header.state_root,
            transactions_root = header.transactions_root,
            receipts_root = header.receipts_root,
        );

        // Update the parent block hash in the state database.
        self.state.database.set_parent_block_header(header);

        Ok(self.state.database.parent_block_header())
    }

    /// Computes the current output root of the executor, based on the parent header and the
    /// state's underlying trie.
    ///
    /// **CONSTRUCTION:**
    /// ```text
    /// output_root = keccak256(version_byte .. payload)
    /// payload = state_root .. withdrawal_storage_root .. latest_block_hash
    /// ```
    ///
    /// ## Returns
    /// - `Ok(output_root)`: The computed output root.
    /// - `Err(_)`: If an error occurred while computing the output root.
    pub fn compute_output_root(&mut self) -> Result<B256> {
        const OUTPUT_ROOT_VERSION: u8 = 0;
        const L2_TO_L1_MESSAGE_PASSER_ADDRESS: Address =
            address!("4200000000000000000000000000000000000016");

        // Fetch the L2 to L1 message passer account from the cache or underlying trie.
        println!("before");
        let storage_root =
            match self.state.database.storage_roots().get(&L2_TO_L1_MESSAGE_PASSER_ADDRESS) {
                Some(storage_root) => storage_root
                    .blinded_commitment()
                    .ok_or(anyhow!("Account storage root is unblinded"))?,
                None => {
                    self.state
                        .database
                        .get_trie_account(&L2_TO_L1_MESSAGE_PASSER_ADDRESS)?
                        .ok_or(anyhow!("L2 to L1 message passer account not found in trie"))?
                        .storage_root
                }
            };
        println!("after");
        let parent_header = self.state.database.parent_block_header();

        info!(
            target: "client_executor",
            "Computing output root | Version: {version} | State root: {state_root} | Storage root: {storage_root} | Block hash: {hash}",
            version = OUTPUT_ROOT_VERSION,
            state_root = self.state.database.parent_block_header().state_root,
            hash = parent_header.seal(),
        );

        // Construct the raw output.
        let mut raw_output = [0u8; 128];
        raw_output[31] = OUTPUT_ROOT_VERSION;
        raw_output[32..64].copy_from_slice(parent_header.state_root.as_ref());
        raw_output[64..96].copy_from_slice(storage_root.as_ref());
        raw_output[96..128].copy_from_slice(parent_header.seal().as_ref());
        let output_root = keccak256(raw_output);

        info!(
            target: "client_executor",
            "Computed output root for block # {block_number} | Output root: {output_root}",
            block_number = parent_header.number,
        );

        // Hash the output and return
        Ok(output_root)
    }

    /// Returns the active [SpecId] for the executor.
    ///
    /// ## Takes
    /// - `timestamp`: The timestamp of the executing block.
    ///
    /// ## Returns
    /// The active [SpecId] for the executor.
    fn revm_spec_id(&self, timestamp: u64) -> SpecId {
        if self.config.is_fjord_active(timestamp) {
            SpecId::FJORD
        } else if self.config.is_ecotone_active(timestamp) {
            SpecId::ECOTONE
        } else if self.config.is_canyon_active(timestamp) {
            SpecId::CANYON
        } else if self.config.is_regolith_active(timestamp) {
            SpecId::REGOLITH
        } else {
            SpecId::BEDROCK
        }
    }

    /// Returns the active [CfgEnvWithHandlerCfg] for the executor.
    ///
    /// ## Takes
    /// - `timestamp`: The timestamp of the executing block.
    ///
    /// ## Returns
    /// The active [CfgEnvWithHandlerCfg] for the executor.
    fn evm_cfg_env(&self, timestamp: u64) -> CfgEnvWithHandlerCfg {
        let cfg_env = CfgEnv::default().with_chain_id(self.config.l2_chain_id);
        let mut cfg_handler_env =
            CfgEnvWithHandlerCfg::new_with_spec_id(cfg_env, self.revm_spec_id(timestamp));
        cfg_handler_env.enable_optimism();
        cfg_handler_env
    }

    /// Computes the receipts root from the given set of receipts.
    ///
    /// ## Takes
    /// - `receipts`: The receipts to compute the root for.
    /// - `config`: The rollup config to use for the computation.
    /// - `timestamp`: The timestamp to use for the computation.
    ///
    /// ## Returns
    /// The computed receipts root.
    fn compute_receipts_root(
        receipts: &[OpReceiptEnvelope],
        config: &RollupConfig,
        timestamp: u64,
    ) -> B256 {
        // There is a minor bug in op-geth and op-erigon where in the Regolith hardfork,
        // the receipt root calculation does not inclide the deposit nonce in the
        // receipt encoding. In the Regolith hardfork, we must strip the deposit nonce
        // from the receipt encoding to match the receipt root calculation.
        if config.is_regolith_active(timestamp) && !config.is_canyon_active(timestamp) {
            let receipts = receipts
                .iter()
                .cloned()
                .map(|receipt| match receipt {
                    OpReceiptEnvelope::Deposit(mut deposit_receipt) => {
                        deposit_receipt.receipt.deposit_nonce = None;
                        OpReceiptEnvelope::Deposit(deposit_receipt)
                    }
                    _ => receipt,
                })
                .collect::<Vec<_>>();

            ordered_trie_with_encoder(receipts.as_ref(), |receipt, mut buf| {
                receipt.encode_2718(&mut buf)
            })
            .root()
        } else {
            ordered_trie_with_encoder(receipts, |receipt, mut buf| receipt.encode_2718(&mut buf))
                .root()
        }
    }

    /// Computes the transactions root from the given set of encoded transactions.
    ///
    /// ## Takes
    /// - `transactions`: The transactions to compute the root for.
    ///
    /// ## Returns
    /// The computed transactions root.
    fn compute_transactions_root(transactions: &[RawTransaction]) -> B256 {
        ordered_trie_with_encoder(transactions, |tx, buf| buf.put_slice(tx.as_ref())).root()
    }

    /// Prepares a [BlockEnv] with the given [L2PayloadAttributes].
    ///
    /// ## Takes
    /// - `payload`: The payload to prepare the environment for.
    /// - `env`: The block environment to prepare.
    fn prepare_block_env(
        spec_id: SpecId,
        config: &RollupConfig,
        parent_header: &Header,
        payload_attrs: &L2PayloadAttributes,
    ) -> BlockEnv {
        let blob_excess_gas_and_price = parent_header
            .next_block_excess_blob_gas()
            .or_else(|| spec_id.is_enabled_in(SpecId::ECOTONE).then_some(0))
            .map(|x| BlobExcessGasAndPrice::new(x as u64));
        // If the payload attribute timestamp is past canyon activation,
        // use the canyon base fee params from the rollup config.
        let base_fee_params = if config.is_canyon_active(payload_attrs.timestamp) {
            config.canyon_base_fee_params.expect("Canyon base fee params not provided")
        } else {
            config.base_fee_params
        };
        let next_block_base_fee =
            parent_header.next_block_base_fee(base_fee_params).unwrap_or_default();

        BlockEnv {
            number: U256::from(parent_header.number + 1),
            coinbase: address!("4200000000000000000000000000000000000011"),
            timestamp: U256::from(payload_attrs.timestamp),
            gas_limit: U256::from(payload_attrs.gas_limit.expect("Gas limit not provided")),
            basefee: U256::from(next_block_base_fee),
            difficulty: U256::ZERO,
            prevrandao: Some(payload_attrs.prev_randao),
            blob_excess_gas_and_price,
        }
    }

    /// Prepares a [TxEnv] with the given [OpTxEnvelope].
    ///
    /// ## Takes
    /// - `transaction`: The transaction to prepare the environment for.
    /// - `env`: The transaction environment to prepare.
    ///
    /// ## Returns
    /// - `Ok(())` if the environment was successfully prepared.
    /// - `Err(_)` if an error occurred while preparing the environment.
    fn prepare_tx_env(transaction: &OpTxEnvelope, encoded_transaction: &[u8]) -> Result<TxEnv> {
        let mut env = TxEnv::default();
        match transaction {
            OpTxEnvelope::Legacy(signed_tx) => {
                let tx = signed_tx.tx();
                env.caller = signed_tx
                    .recover_signer()
                    .map_err(|e| anyhow!("Failed to recover signer: {}", e))?;
                env.gas_limit = tx.gas_limit as u64;
                env.gas_price = U256::from(tx.gas_price);
                env.gas_priority_fee = None;
                env.transact_to = match tx.to {
                    TxKind::Call(to) => TransactTo::Call(to),
                    TxKind::Create => TransactTo::Create,
                };
                env.value = tx.value;
                env.data = tx.input.clone();
                env.chain_id = tx.chain_id;
                env.nonce = Some(tx.nonce);
                env.access_list.clear();
                env.blob_hashes.clear();
                env.max_fee_per_blob_gas.take();
                env.optimism = OptimismFields {
                    source_hash: None,
                    mint: None,
                    is_system_transaction: Some(false),
                    enveloped_tx: Some(encoded_transaction.to_vec().into()),
                };
                Ok(env)
            }
            OpTxEnvelope::Eip2930(signed_tx) => {
                let tx = signed_tx.tx();
                env.caller = signed_tx
                    .recover_signer()
                    .map_err(|e| anyhow!("Failed to recover signer: {}", e))?;
                env.gas_limit = tx.gas_limit as u64;
                env.gas_price = U256::from(tx.gas_price);
                env.gas_priority_fee = None;
                env.transact_to = match tx.to {
                    TxKind::Call(to) => TransactTo::Call(to),
                    TxKind::Create => TransactTo::Create,
                };
                env.value = tx.value;
                env.data = tx.input.clone();
                env.chain_id = Some(tx.chain_id);
                env.nonce = Some(tx.nonce);
                env.access_list = tx
                    .access_list
                    .0
                    .iter()
                    .map(|l| {
                        (
                            l.address,
                            l.storage_keys.iter().map(|k| U256::from_be_bytes(k.0)).collect(),
                        )
                    })
                    .collect();
                env.blob_hashes.clear();
                env.max_fee_per_blob_gas.take();
                env.optimism = OptimismFields {
                    source_hash: None,
                    mint: None,
                    is_system_transaction: Some(false),
                    enveloped_tx: Some(encoded_transaction.to_vec().into()),
                };
                Ok(env)
            }
            OpTxEnvelope::Eip1559(signed_tx) => {
                let tx = signed_tx.tx();
                env.caller = signed_tx
                    .recover_signer()
                    .map_err(|e| anyhow!("Failed to recover signer: {}", e))?;
                env.gas_limit = tx.gas_limit as u64;
                env.gas_price = U256::from(tx.max_fee_per_gas);
                env.gas_priority_fee = Some(U256::from(tx.max_priority_fee_per_gas));
                env.transact_to = match tx.to {
                    TxKind::Call(to) => TransactTo::Call(to),
                    TxKind::Create => TransactTo::Create,
                };
                env.value = tx.value;
                env.data = tx.input.clone();
                env.chain_id = Some(tx.chain_id);
                env.nonce = Some(tx.nonce);
                env.access_list = tx
                    .access_list
                    .0
                    .iter()
                    .map(|l| {
                        (
                            l.address,
                            l.storage_keys.iter().map(|k| U256::from_be_bytes(k.0)).collect(),
                        )
                    })
                    .collect();
                env.blob_hashes.clear();
                env.max_fee_per_blob_gas.take();
                env.optimism = OptimismFields {
                    source_hash: None,
                    mint: None,
                    is_system_transaction: Some(false),
                    enveloped_tx: Some(encoded_transaction.to_vec().into()),
                };
                Ok(env)
            }
            OpTxEnvelope::Deposit(tx) => {
                env.caller = tx.from;
                env.access_list.clear();
                env.gas_limit = tx.gas_limit as u64;
                env.gas_price = U256::ZERO;
                env.gas_priority_fee = None;
                match tx.to {
                    TxKind::Call(to) => env.transact_to = TransactTo::Call(to),
                    TxKind::Create => env.transact_to = TransactTo::Create,
                }
                env.value = tx.value;
                env.data = tx.input.clone();
                env.chain_id = None;
                env.nonce = None;
                env.optimism = OptimismFields {
                    source_hash: Some(tx.source_hash),
                    mint: tx.mint,
                    is_system_transaction: Some(tx.is_system_transaction),
                    enveloped_tx: Some(encoded_transaction.to_vec().into()),
                };
                Ok(env)
            }
            _ => anyhow::bail!("Unexpected tx type"),
        }
    }
}

#[cfg(test)]
mod test {
    extern crate std;

    use super::*;
    use alloy_primitives::{address, b256, hex};
    use alloy_rlp::Decodable;
    use kona_derive::types::{OP_BASE_FEE_PARAMS, OP_CANYON_BASE_FEE_PARAMS};
    use kona_mpt::NoopTrieDBHinter;
    use serde::Deserialize;
    use std::{collections::HashMap, format};

    /// A [TrieDBFetcher] implementation that fetches trie nodes and bytecode from the local
    /// testdata folder.
    #[derive(Deserialize)]
    struct TestdataTrieDBFetcher {
        preimages: HashMap<B256, Bytes>,
    }

    impl TestdataTrieDBFetcher {
        /// Constructs a new [TestdataTrieDBFetcher] with the given testdata folder.
        pub(crate) fn new(testdata_folder: &str) -> Self {
            let file_name = format!("testdata/{}/output.json", testdata_folder);
            let preimages = serde_json::from_str::<HashMap<B256, Bytes>>(
                &std::fs::read_to_string(&file_name).unwrap(),
            )
            .unwrap();
            Self { preimages }
        }
    }

    impl TrieDBFetcher for TestdataTrieDBFetcher {
        fn trie_node_preimage(&self, key: B256) -> Result<Bytes> {
            self.preimages
                .get(&key)
                .cloned()
                .ok_or_else(|| anyhow!("Preimage not found for key: {}", key))
        }

        fn bytecode_by_hash(&self, code_hash: B256) -> Result<Bytes> {
            self.preimages
                .get(&code_hash)
                .cloned()
                .ok_or_else(|| anyhow!("Bytecode not found for hash: {}", code_hash))
        }

        fn header_by_hash(&self, hash: B256) -> Result<Header> {
            let encoded_header = self
                .preimages
                .get(&hash)
                .ok_or_else(|| anyhow!("Header not found for hash: {}", hash))?;
            Header::decode(&mut encoded_header.as_ref()).map_err(|e| anyhow!(e))
        }
    }

    #[test]
    fn test_l2_block_executor_small_block() {
        // Static for the execution of block #120794432 on OP mainnet.
        // https://optimistic.etherscan.io/block/120794432

        // Make a mock rollup config, with Ecotone activated at timestamp = 0.
        let rollup_config = RollupConfig {
            l2_chain_id: 10,
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            base_fee_params: OP_BASE_FEE_PARAMS,
            canyon_base_fee_params: Some(OP_CANYON_BASE_FEE_PARAMS),
            ..Default::default()
        };

        // Decode the headers.
        let raw_header = hex!("f90244a0ff7c6abc94edcaddd02c12ec7d85ffbb3ba293f3b76897e4adece57e692bcc39a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0a0b24abb13d6149947247a8817517971bb8d213de1e23225e2b20d36a5b6427ca0c31e4a2ada52ac698643357ca89ef2740d384076ef0e17b653bcb6ea7dd8902ea09f4fcf34e78afc216240e3faa72c822f8eea4757932eb9e0fd42839d192bb903b901000440000210068007000000940000000220000006000820048404800002000004040100001b2000008800001040000018280000400001200004000101086000000802800080004008010001080000200100a00000204840000118042080000400804001000a0400080200111000000800050000020200064000000012000800048000000000101800200002000000080008001581402002200210341089000080c2d004106000000018000000804285800800000020000180008000020000000000020103410400000000200400008000280400000100020000002002000021000811000920808000010000000200210400000020008000400000000000211008808407332d3f8401c9c3808327c44d84665a343780a0edba75784acf3165bffd96df8b78ffdb3781db91f886f22b4bee0a6f722df93988000000000000000083202ef8a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a0917693152c4a041efbc196e9d169087093336da96a8bb3af1e55fce447a7b8a9");
        let header = Header::decode(&mut &raw_header[..]).unwrap();
        let raw_expected_header = hex!("f90243a09506905902f5c3613c5441a8697c09e7aafdb64082924d8bd2857f9e34a47a9aa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0a1e9207c3c68cd4854074f08226a3643debed27e45bf1b22ab528f8de16245eda0121e8765953af84974b845fd9b01f5ff9b0f7d2886a2464535e8e9976a1c8daba092c6a5e34d7296d63d1698258c40539a20080c668fc9d63332363cfbdfa37976b9010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000808407332d408401c9c38082ab4b84665a343980a0edba75784acf3165bffd96df8b78ffdb3781db91f886f22b4bee0a6f722df93988000000000000000083201f31a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a0917693152c4a041efbc196e9d169087093336da96a8bb3af1e55fce447a7b8a9");
        let expected_header = Header::decode(&mut &raw_expected_header[..]).unwrap();

        // Initialize the block executor on block #120794431's post-state.
        let mut l2_block_executor = StatelessL2BlockExecutor::builder(&rollup_config)
            .with_parent_header(header.seal_slow())
            .with_fetcher(TestdataTrieDBFetcher::new("block_120794432_exec"))
            .with_hinter(NoopTrieDBHinter)
            .with_precompile_overrides(NoPrecompileOverride)
            .build()
            .unwrap();

        let raw_tx = hex!("7ef8f8a003b511b9b71520cd62cad3b5fd5b1b8eaebd658447723c31c7f1eba87cfe98c894deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e2000000558000c5fc5000000000000000300000000665a33a70000000001310e960000000000000000000000000000000000000000000000000000000214d2697300000000000000000000000000000000000000000000000000000000000000015346d208a396843018a2e666c8e7832067358433fb87ca421273c6a4e69f78d50000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985");
        let payload_attrs = L2PayloadAttributes {
            fee_recipient: address!("4200000000000000000000000000000000000011"),
            gas_limit: Some(0x1c9c380),
            timestamp: 0x665a3439,
            prev_randao: b256!("edba75784acf3165bffd96df8b78ffdb3781db91f886f22b4bee0a6f722df939"),
            withdrawals: Default::default(),
            parent_beacon_block_root: Some(b256!(
                "917693152c4a041efbc196e9d169087093336da96a8bb3af1e55fce447a7b8a9"
            )),
            transactions: alloc::vec![raw_tx.into()],
            no_tx_pool: false,
        };
        let produced_header = l2_block_executor.execute_payload(payload_attrs).unwrap().clone();

        assert_eq!(produced_header, expected_header);
        assert_eq!(
            l2_block_executor.state.database.parent_block_header().seal(),
            expected_header.hash_slow()
        );
    }

    #[test]
    fn test_l2_block_executor_small_block_2() {
        // Static for the execution of block #121049889 on OP mainnet.
        // https://optimistic.etherscan.io/block/121049889

        // Make a mock rollup config, with Ecotone activated at timestamp = 0.
        let rollup_config = RollupConfig {
            l2_chain_id: 10,
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            base_fee_params: OP_BASE_FEE_PARAMS,
            canyon_base_fee_params: Some(OP_CANYON_BASE_FEE_PARAMS),
            ..Default::default()
        };

        // Decode the headers.
        let raw_parent_header = hex!("f90245a0311e3aa67dca0d157b8e8a4e117a4fd34cedcebc63f5708976e86581c07824a5a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0b1772b8cd400c2d2cfee5bd294bcc399e4c8330d856907f95d2305a64ff9c968a0a42b2ec1d1e928f2b63224888d482f72537ee392e98390c760c902ca3f7d75d8a0e993b3cac72163177e7e728c5e4d074551b181a45f49b0026c48e893f7b4768eb901008008140067b0392a00048280488c10a04000180084400038834008020400c960003c9000068083b00000f00cc40088ab48306c402008068f0810881b84342000860104c10500102b209410584214804a40034000080d622018042ca008000204a016089206020412050c1902440158505802207070800900020028facaacc0101e0a08000010a003a15166a231024090841918038500ac4082281880810648221200881000116002c0444044421024c6c401c0008d42280c98408085142c3041542272832790b4154e66c082080a2090100002409548047010c208220588622694900120454200800600104100e01a160214408c4000141890022802209102488084073713208401c9c380831f42d1846661fff980a02ea5360883566f7bf998c6ce46367b64aeb24c0178a6e5752ea796ca9b9f951988000000000000000084038e4654a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a0025cfb4d23d2384982b73c2669eeb4fb73b29960750554e2380af54add10dbda");
        let parent_header = Header::decode(&mut &raw_parent_header[..]).unwrap();
        let raw_expected_header = hex!("f90245a0925b8e3c7216dd1c62e3fd9911f6cb3f456b9aa685f34239180d1a7ef7653b7fa01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0ac6f1a9722101300ba71fb58517eadbb4964dc4f4891f8f3e58a292e7c3204f3a032ae1c22601d63eaa26aa5ab30e6b8ae1cdfb7104c0067327d91bc3094461fc9a016c68c81160c03fa72763fdd578c6a5563cca47ded1a54df3610c0412b976b25b90100000004000000000000000000001000000001200000000000000000000040000000001000200000000000000000000000200000000000000001000000000420000200000200002000000000800000000000000000000400000000000000012000000000000200000040008400050009000000000000000000000000000200050000000000000000000000010000000000000050840000000000000000000010000000000400000000000000000000008000000000010000000000000000000804000000000008000001000010000000000000840000080000100000000000600000000000000000002100000000000000001000000000008000000800000000008084073713218401c9c380830505a2846661fffb80a0d91ae18a8b94471ef1b15686ef8a6144a109b837c28488a0f1a2a4e4ad29d5af88000000000000000084038c2024a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a05e7da14ac6b18e62306c84d9d555387d4b4a6c3d122df22a2af2b68bf219860d");
        let expected_header = Header::decode(&mut &raw_expected_header[..]).unwrap();

        // Initialize the block executor on block #121049888's post-state.
        let mut l2_block_executor = StatelessL2BlockExecutor::builder(&rollup_config)
            .with_parent_header(parent_header.seal_slow())
            .with_fetcher(TestdataTrieDBFetcher::new("block_121049889_exec"))
            .with_hinter(NoopTrieDBHinter)
            .with_precompile_overrides(NoPrecompileOverride)
            .build()
            .unwrap();

        let raw_txs = alloc::vec![
            hex!("7ef8f8a01e6036fa5dc5d76e0095f42fef2c4aa7d6589b4f496f9c4bea53daef1b4a24c194deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e2000000558000c5fc50000000000000000000000006661ff73000000000131b40700000000000000000000000000000000000000000000000000000005c9ea450a0000000000000000000000000000000000000000000000000000000000000001e885b088376fedbd0490a7991be47854872f6467c476d255eed3151d5f6a95940000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985").into(),
            hex!("02f9010b0a8301b419835009eb840439574783030fc3940000000000002bdbf1bf3279983603ec279cc6df8702c2ad68fd9000b89666e0daa0001e80001ec0001d0220001e01001e01000bfe1561df392590b0cb3ec093b711066774ca96cd001e01001e20001ee49dbb844d000b3678862f04290e565cca2ef163baeb92bb76790c001e01001e01001ea0000b38873c13509d36077a4638183f4a9a72f8a66b91001e20000bcaaef30cf6e70a0118e59cd3fb88164de6d144b5003a01001802c2ad68fd900000012b817fc001a098c44ee6585f33a4fbc9c999b2469697dd8007b986c79569ae6f3d077de45a1ca035c3ea5e954ae76fdf75f7d7ce215a339ac20a772081b62908d5fcf551693e3a").into(),
            hex!("02f904920a828a19834c4b408403dce3e7837a1200944d75a5ce454b264b187bee9e189af1564a68408d80b90424b1dc65a400018958e0d17c70a7bddf525ee0a3bf00f5c8f886a03156c522c0b256cb884d00000000000000000000000000000000000000000000000000000000001814035a6bc28056dae2cfa8a6479f5e50eee95bb3ae2b65be853a4440f15cb60211ba00000000000000000000000000000000000000000000000000000000000000e0000000000000000000000000000000000000000000000000000000000000026000000000000000000000000000000000000000000000000000000000000003400000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000016000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000b2c639c533813f4aa9d7837caf62653d097ff85000000000000000000000000000000000000000000000000000000e8d4a510000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000606ecf709c09afd92138cca6ee144be81e1c6ef231d4586a22eb7fc801826e837691e208839c1c58d50a31826c8b47c5218c3898ee6671f734bd9b9584ce210e8b1fb287f374f07a99bbce2ddedc655ee5c94f8fee715db21644ae134638af8c32d18b1d27dbc2e12b205ea25ab6bb4ec447ee7f40dba560e511a20fd8a3775d04ad83bf593e3587be1dd85ab9b2053d1386fae00c5fdea56a68ea147b706e5ced65ab296b8d9248aa943787a5c8aa4fd56ba7133d087e84a625fe1c3d8a390b5000000000000000000000000000000000000000000000000000000000000000666634013473fce9d0696d9f0375be4260a81518a85f2482b3f5336848f8fa3ce1a3f7032124577ee2a755122f916e4fe757fc42eb5561216892ed806d368908b69c4d4d1cd06897a3a2f02c17ffba7a762e4cbbdb086a1181f1111874f88f38f3b86fa03508822346a167de3f6afc9066cc274103cf18d62c7d6a4d93dcd000b7842951fd9a14a647148dac543f446cd9427dedbc3c3ca5ed2b36f5c27ce76de46d4291be6ef3b41679501c8f0341d35cf6afc9f7d91d56ad1a8ae34fc0e708ac001a013f549ca84754e18fae518daa617d19dfbdff6da7bc794bab89e7a17288cb5b5a00c4913669beb11412e9e04bd4311ed5b11443b9e34f7fb25488e58047ddd8820").into()
        ];
        let payload_attrs = L2PayloadAttributes {
            fee_recipient: address!("4200000000000000000000000000000000000011"),
            gas_limit: Some(30000000),
            timestamp: 1717698555,
            prev_randao: b256!("d91ae18a8b94471ef1b15686ef8a6144a109b837c28488a0f1a2a4e4ad29d5af"),
            withdrawals: Default::default(),
            parent_beacon_block_root: Some(b256!(
                "5e7da14ac6b18e62306c84d9d555387d4b4a6c3d122df22a2af2b68bf219860d"
            )),
            transactions: raw_txs,
            no_tx_pool: false,
        };
        let produced_header = l2_block_executor.execute_payload(payload_attrs).unwrap().clone();

        assert_eq!(produced_header, expected_header);
        assert_eq!(
            l2_block_executor.state.database.parent_block_header().seal(),
            expected_header.hash_slow()
        );
    }

    #[test]
    fn test_l2_block_executor_small_block_3() {
        // Static for the execution of block #121003241 on OP mainnet.
        // https://optimistic.etherscan.io/block/121003241

        // Make a mock rollup config, with Ecotone activated at timestamp = 0.
        let rollup_config = RollupConfig {
            l2_chain_id: 10,
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            base_fee_params: OP_BASE_FEE_PARAMS,
            canyon_base_fee_params: Some(OP_CANYON_BASE_FEE_PARAMS),
            ..Default::default()
        };

        // Decode the headers.
        let raw_parent_header = hex!("f90245a01fe9a4a3f3a03b5e9bf26739dc0402016bcd0b4eba84f6daec89cd25ede03785a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0f0f4294d35c59be9ac60e3c8b10f72f082eb20db04e84b89622eaf36dc288f94a037567276c3663d85aa9c8f6d9fa3a9b02511a5314c08d83648caae01da377f0da0a5cc7888ada10b0cf445632d9239c129cb55b9822edcc6062262660cc9786457b9010007000032410480052001888000000000000200000400200040040000442002000a892000100000020008001100112000000000408000b012000002c200b48080000068040001480885003408000880010044000010241440800428208400004044000880820800800100100000000801820000000000000081000030000800204000000840000000802a0000000100400004000180300000004120104000001922000102000000000060001289c024840010000521800000000022140000208040001203800420620019020200004000209008009000000000004000880070120010220820502000500400202000000000040028000089c00080100000010008808407365ce88401c9c380832415e9846660938980a022e77867678dc60aace7567ee344620f47a66be343eac90a82bf619ea37de357880000000000000000840398f69aa056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a050f4a35e2f059621cba649e719d23a2a9d030189fd19172a689c76d3adf39fec");
        let parent_header = Header::decode(&mut &raw_parent_header[..]).unwrap();
        let raw_expected_header = hex!("f90245a090957c484fec69a6b308f18d83a320b18a5471ba9566e5b56dfc656abd354744a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a049dfddc9ce6d832c6ab981aea324c3d57b1b1d93823656b43d02608e6b59f3bda0533a1c4f39fa301e354292186123681d97ae64a788cf2af61e6f70e3080c1ac3a0c888d1dfb9590590036630c91d4ff2401a4946524f315bffbbbed795820e3744b90100060000024200002000118880000000008004000104000000000000000400010000080000000000000000040100000000000800c08000200a0000020000200080000000040040000800000008000000000040080004000000804000010002000040802088028c0010000014000200080102001000000800000000001000082000000000002000000000000000000000000044100080200000000100000c00800002000040001100000040100280000400040480000000000000800600000020c040001402008000401001201620020000000000000004000000800200000320000010200200080000400000000000040000000004008080002000000000010000808407365ce98401c9c3808312f8db846660938b80a022e77867678dc60aace7567ee344620f47a66be343eac90a82bf619ea37de3578800000000000000008403970597a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a050f4a35e2f059621cba649e719d23a2a9d030189fd19172a689c76d3adf39fec");
        let expected_header = Header::decode(&mut &raw_expected_header[..]).unwrap();

        // Initialize the block executor on block #121003240's post-state.
        let mut l2_block_executor = StatelessL2BlockExecutor::builder(&rollup_config)
            .with_parent_header(parent_header.seal_slow())
            .with_fetcher(TestdataTrieDBFetcher::new("block_121003241_exec"))
            .with_hinter(NoopTrieDBHinter)
            .with_precompile_overrides(NoPrecompileOverride)
            .build()
            .unwrap();

        let raw_txs = alloc::vec![
            hex!("7ef8f8a02c3adbd572915b3ef2fe7c81418461cb32407df8cb1bd4c1f5f4b45e474bfce694deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e2000000558000c5fc5000000000000000400000000666092ff00000000013195d800000000000000000000000000000000000000000000000000000004da0e1101000000000000000000000000000000000000000000000000000000000000000493a1359bf7a89d8b2b2073a153c47f9c399f8f7a864e4f25744d6832cb6fadd80000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985").into(),
            hex!("f86a03840998d150827b0c9422fb762f614ede47d33ca2de13a5fb16354a7a5b872defc438f220008038a0e83ca5fd673c57230b1ea308752959568a795fc0b2eccc4128bb295673f4f576a04de60eb10a6aa6fcffd5a956523a92451b06cf669cf332139ac2937880e4ee2f").into(),
            hex!("f87e8301abd284050d2c55830493e094a43305ce0164d87d7b2368f91a1dcc4ebda751278097c201015dc7073aac5a2702007a6c235e4c4f676660938937a07575b3c2ed04981845adc29fc27bf573ccd17462c2d5789e3844d66d29277a79a005175e178a234d48c7e15bfaa979f1b78636228d550a200d9e34e05169d1b770").into(),
            hex!("02f90fb40a83136342840104b33a840836a06e830995ae94087000a300de7200382b55d40045000000e5d60e80b90f4482ad56cb000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000042000000000000000000000000000000000000000000000000000000000000007c00000000000000000000000000000000000000000000000000000000000000b600000000000000000000000008f7dbe4fa3818025d82bb10190f178eaf5992bea0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000003046a761202000000000000000000000000b5fbfeba9848664fd1a49dc2a250d9b5d1294f2a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002800000000000000000000000000000000000000000000000000000000000000104414bf389000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000007f5c764cbc14f9669b88837ca1490cca17c3160700000000000000000000000000000000000000000000000000000000000027100000000000000000000000008f7dbe4fa3818025d82bb10190f178eaf5992bea000000000000000000000000000000000000000000000000000000006660a175000000000000000000000000000000000000000000000000de0b6b3a764000000000000000000000000000000000000000000000000000000000000004a71a1f00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000419a434a72274666c423432aad2ffb19565424d0c6e2d17fc64934b3e4fec97788446afa2d830e2dd926c04ce882e601cb9fa398149b5d778cbe3ebe6038e8643e1b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000a34049de917233a7516aa01fc0bad683a6a8b29d0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000003046a761202000000000000000000000000b5fbfeba9848664fd1a49dc2a250d9b5d1294f2a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002800000000000000000000000000000000000000000000000000000000000000104414bf389000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000007f5c764cbc14f9669b88837ca1490cca17c316070000000000000000000000000000000000000000000000000000000000002710000000000000000000000000a34049de917233a7516aa01fc0bad683a6a8b29d000000000000000000000000000000000000000000000000000000006660a17b0000000000000000000000000000000000000000000000002870624346de10000000000000000000000000000000000000000000000000000000000000d8ecb600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000418217f8941b74fc2cd49b297652e34ba54465a905ccc5fd452b48fd40a82502590c4c48e64b2a0f0e8e8793a13addfe6d4937bf78d9875a4d9002266be5ecc0a41b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000fb5049c82e7fa9e7011ddd435b30652b48a1195b0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000003046a761202000000000000000000000000b5fbfeba9848664fd1a49dc2a250d9b5d1294f2a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002800000000000000000000000000000000000000000000000000000000000000104414bf3890000000000000000000000007f5c764cbc14f9669b88837ca1490cca17c31607000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000002710000000000000000000000000fb5049c82e7fa9e7011ddd435b30652b48a1195b000000000000000000000000000000000000000000000000000000006660a1890000000000000000000000000000000000000000000000000000000000d019f10000000000000000000000000000000000000000000000002543ff48d0da90eb00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000413698ad34509d153bf3d7287553d81c098983d590f5c9e80c95c361de3c220c745eafd0ca4ef4e78cffe29e7b346ee3d134d20eebd9d98663438646a1ea3801d61c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000009ef549707a5d504c24b0627aff2eb845e8ae02d80000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000003046a761202000000000000000000000000b5fbfeba9848664fd1a49dc2a250d9b5d1294f2a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002800000000000000000000000000000000000000000000000000000000000000104414bf38900000000000000000000000068f180fcce6836688e9084f035309e29bf0a20950000000000000000000000007f5c764cbc14f9669b88837ca1490cca17c3160700000000000000000000000000000000000000000000000000000000000001f40000000000000000000000009ef549707a5d504c24b0627aff2eb845e8ae02d8000000000000000000000000000000000000000000000000000000006660a188000000000000000000000000000000000000000000000000000000000000048200000000000000000000000000000000000000000000000000000000000c78cc0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041d561852d56b0baac02af7a38ac72d7f560d4a0956032e051adb598fbdb035661280071192a277daf0d36667dc88155f9b445a465dbbadc3149b3ee6c07ae905d1c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c080a0f2c4eec1941db4f698a0fc5b24d708d4231decf19719977bca15af04cbd39cc6a022036042105c9ede61cf13552f6c2d712a3eefbb4f47df3cbe3d3b9b46723398").into(),
            hex!("02f8af0a8083989680840578b8db83025dbe94dc6ff44d5d932cbd77b52e5612ba0529dc6226f180b844a9059cbb00000000000000000000000056c38d1b4676c9c2259d0820dcbce069d3321d5f00000000000000000000000000000000000000000000000029563f7ac07ae000c080a0d0b1d61b918d88059cc8dbee2833c2ce78573b76c731e266d110ed330fb72563a05ca02995f5ec74c0bd9b7209785d75369a1f43a5f045189a51f851ea9b9a791b").into(),
            hex!("02f8740a832c6a52834c4b4085012a05f200825208948c1e1a0b0f9420139e12fa1379b6a76d381d7c8f870a18f74161700080c001a00b7dcc69c346c674167fdd0cee4b13622838d4d9a1f64ef0270d366e61c49fdaa02d99fcd56b7ef8aec6a04c0204a6fd66dcddb755cd54226527a51e5ba22aacd7").into(),
            hex!("f86a808403b23254825208945e809a85aa182a9921edd10a4163745bb3e362848704f7793d6560098038a0c921dce37651444a6c3004e85263d7ef593225d6f5a6ac19265c5a1044f598caa003cbfcc7b3d89a023c7d423496bc0f55c281c501cdd00909e6e09485d90d6500").into(),
            hex!("f8aa8207a88403a9e89182cac994dc6ff44d5d932cbd77b52e5612ba0529dc6226f180b844a9059cbb0000000000000000000000002e2927d05851ae228ab68dd04434dece401cf72b00000000000000000000000000000000000000000000000029998b20cdd0c00038a0a3d6514ad022c5b79f8b41cb59b7e48b62ca90d409a5438783f89947009a548ea037de75cc680392eac97820b5884239ca0a0a990e63fc118b0040b631ac73fc52").into(),
            hex!("02f905720a820a1e830f42408404606a2c83044bc0940000000071727de22e5e9d8baf0edac6f37da03280b90504765e827f00000000000000000000000000000000000000000000000000000000000000400000000000000000000000004337016838785634c63fce393bfc6222564436c4000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000200000000000000000000000006a2aad2e20ef62b5b56e4e2b5e342e53ee7fa04f000017719c140000000000000000000000000000000005300000000000000002000000000000000000000000000000000000000000000000000000000000012000000000000000000000000000000000000000000000000000000000000001400000000000000000000000000002265a0000000000000000000000000001d4c00000000000000000000000000000000000000000000000000000000000010a370000000000000000000000000010c8e000000000000000000000000004d4157c00000000000000000000000000000000000000000000000000000000000003200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001a4e9ae5c530100000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000001400000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000200000000000000000000000006668bc6eea73404b4da5775c774fafc815b66b36000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000044a9059cbb000000000000000000000000efe1bfc13a0f086066fbe23a18c896eb697ca5cc00000000000000000000000000000000000000000000000000000001a13b8600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000b59d0021a869f1ed3a661ffe8c9b41ec6244261d9800000000000000000000000000004e8a0000000000000000000000000000000100000000000000000000000000000000000000000000000000000000666095e00000000000000000000000000000000000000000000000000000000000000000dcc3f422395fc31d9308eb3c4805623ddc445433eb04f7d4d7b07a9b4abb16886820d7c9a50f7bb450cff51271a9ff789322e9a72c65cf58da188c6b77093fdb1b00000000000000000000000000000000000000000000000000000000000000000000000000000000000042fff34f0b4b601ea1d21ac1184895b6d6b81662b95d14e59dfb768ef963838ca29f67dcaf0423b47312bd82d9f498976b28765bec3e79153ca76f644f04ef14dc001b000000000000000000000000000000000000000000000000000000000000c001a0ccd6f3e292c0acaea26b3fd6fee4bc1840fd38553b01637e01990ade4b6b26d4a05daf9fa73f7c0c0ae24097e01d04ed2d6548cd9a3668f8aa18abdb5eca623e08").into(),
            hex!("02f901920a820112830c5c06840af2724a830473c694a062ae8a9c5e11aaa026fc2670b0d65ccc8b285880b901245a47ddc3000000000000000000000000cb8fa9a76b8e203d8c3797bf438d8fb81ea3326a0000000000000000000000008ae125e8653821e851f12a49f7765db9a9ce73840000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000564edf7ae333278800000000000000000000000000000000000000000000000033f7ab48c542f25d000000000000000000000000000000000000000000000000564ca9d9ed92184200000000000000000000000000000000000000000000000033f656b5d849c5b30000000000000000000000004049d8f3f83365555e55e3594993fbeb30ccdc350000000000000000000000000000000000000000000000000000000066609a8ac080a071ef15fac388b7c5c9b56282610f0c7c5bde00ec3dcb07121fa04c64a0c53ccea0746f4a4cf21cf08f75ae7c078efcf148f910000986add1b7998d81874f5de009").into(),
        ];
        let payload_attrs = L2PayloadAttributes {
            fee_recipient: address!("4200000000000000000000000000000000000011"),
            gas_limit: Some(0x1c9c380),
            timestamp: 0x6660938b,
            prev_randao: b256!("22e77867678dc60aace7567ee344620f47a66be343eac90a82bf619ea37de357"),
            withdrawals: Default::default(),
            parent_beacon_block_root: Some(b256!(
                "50f4a35e2f059621cba649e719d23a2a9d030189fd19172a689c76d3adf39fec"
            )),
            transactions: raw_txs,
            no_tx_pool: false,
        };
        let produced_header = l2_block_executor.execute_payload(payload_attrs).unwrap().clone();

        assert_eq!(produced_header, expected_header);
        assert_eq!(
            l2_block_executor.state.database.parent_block_header().seal(),
            expected_header.hash_slow()
        );
    }

    #[test]
    fn test_l2_block_executor_med_block_2() {
        // Static for the execution of block #121057303 on OP mainnet.
        // https://optimistic.etherscan.io/block/121057303/

        // Make a mock rollup config, with Ecotone activated at timestamp = 0.
        let rollup_config = RollupConfig {
            l2_chain_id: 10,
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            base_fee_params: OP_BASE_FEE_PARAMS,
            canyon_base_fee_params: Some(OP_CANYON_BASE_FEE_PARAMS),
            ..Default::default()
        };

        // Decode the headers.
        let raw_parent_header = hex!("f90245a071101c6ce251190d11965257bf7f3b079d5af139a80ec1d2541110ded5da9bd6a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0df99471388344de2cff6b0ff98f9c66429c94f055d0aa4b96f5c5064c47e8ac0a0ebbb62603141a37336a38057ec8eca40e5aea904dafdff82a93c72d0ab9671cea05064f082249a9a7b00c8fc287a6e943b38ba6fe8e1fdc4bb0c10c89b9286a938b9010088000000c0120200100410c08048120b528040a00000000808840180040800201484b4c800040300208020c0001a08014040004021c0000028108018a980614100494020b00008004e020048800088004088094100094180406000c006564401001400005a00080006c0040348030a400a02810f08060104002410910001000011509000050a8200004000000820000280145a10a84000821000c080110020000404000000002e100090b0840000ac2214042040002024084081102800100010d1009226090008900820828280002400808d83a20000187001036005294c60085445800b8000410000a00200c1b19470000000049001052600300100020108808084073730168401c9c3808321106784666239e580a0d8ecef54b9a072a935b297c177b54dbbd5ee9e0fd811a2b69de4b1f28656ad16880000000000000000840392cf07a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a0fa918fbee01a47f475d70995e78b4505bd8714962012720cab27f7e66ec4ea5b");
        let parent_header = Header::decode(&mut &raw_parent_header[..]).unwrap();
        let raw_expected_header = hex!("f90245a0e2608bb1dd6e93302da709acfb82782ee2dcdcbaafdd07fa581958d4d0193560a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0c8286187544a27fdd14372a0182b366be0c0f0f4c4a0a2ef31ee4538972266f5a08799d21d8d3e65106c57a16ea61b4d5ad8e440753b2788e1b8fdec17d6a88c72a06de5e10918168a54b43414e95a4c965baf0bf84c0c11c0711363f663a76c02b8b901000220004001000000000100000000000000000000000010000004000000000000000000c0008000000020001000000800000000000000200200002040000000000000080010000809000020080000000000040000000000000000000000008000000000000000000004000000020000200000000000000000020100100008002000000000000000000000000000000000000020000020000100000000000000000000001000000000000004000000040000000000000010000000000000100000000000020000040000000000000000000000000000000000000000000000000000000008000000000004000000000000000000000000081000000000000000008084073730178401c9c3808306757184666239e780a0d8ecef54b9a072a935b297c177b54dbbd5ee9e0fd811a2b69de4b1f28656ad16880000000000000000840390bc3da056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a0fa918fbee01a47f475d70995e78b4505bd8714962012720cab27f7e66ec4ea5b");
        let expected_header = Header::decode(&mut &raw_expected_header[..]).unwrap();

        // Initialize the block executor on block #121057302's post-state.
        let mut l2_block_executor = StatelessL2BlockExecutor::builder(&rollup_config)
            .with_parent_header(parent_header.seal_slow())
            .with_fetcher(TestdataTrieDBFetcher::new("block_121057303_exec"))
            .with_hinter(NoopTrieDBHinter)
            .with_precompile_overrides(NoPrecompileOverride)
            .build()
            .unwrap();

        let raw_txs = alloc::vec![
            hex!("7ef8f8a01a2c45522a69a90b583aa08a0968847a6fbbdc5480fe6f967b5fcb9384f46e9594deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e2000000558000c5fc500000000000000010000000066623963000000000131b8d700000000000000000000000000000000000000000000000000000003ec02c0240000000000000000000000000000000000000000000000000000000000000001c10a3bb5847ad354f9a70b56f253baaea1c3841647851c4c62e10b22fe4e86940000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985").into(),
            hex!("02f8b40a8316b3cf8405f5e100850bdfd63e00830249f09494b008aa00579c1307b0ef2c499ad98a8ce58e5880b844a9059cbb0000000000000000000000006713cbd38b831255b60b6c28cbdd15c769baad6d0000000000000000000000000000000000000000000000000000000024a12a1ec001a065ae43157da3a4f80cf3a63f572b408cde608af3f4cd98783d8277414d842b72a070caa5b8fcda2f1e9f40f8b310acbe57b95dbcd8f285775b7e53d783539beb94").into(),
            hex!("f9032d8301c3338406244dd88304c7fc941111111254eeb25477b68fb85ed929f73a96058280b902c412aa3caf000000000000000000000000b63aae6c353636d66df13b89ba4425cfe13d10ba000000000000000000000000420000000000000000000000000000000000000600000000000000000000000068f180fcce6836688e9084f035309e29bf0a2095000000000000000000000000b63aae6c353636d66df13b89ba4425cfe13d10ba0000000000000000000000003f343211f0487eb43af2e0e773ba012015e6651a000000000000000000000000000000000000000000000000074a17b261ebbf4000000000000000000000000000000000000000000000000000000000002b13e70000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000001800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001120000000000000000000000000000000000000000000000000000000000f400a0c9e75c48000000000000000020120000000000000000000000000000000000000000000000000000c600006302a000000000000000000000000000000000000000000000000000000000000f5b3fee63c1e581e1b9cc9cc17616ce81f0fa5b958d36f789fb2c0042000000000000000000000000000000000000061111111254eeb25477b68fb85ed929f73a96058202a000000000000000000000000000000000000000000000000000000000001b4ccdee63c1e58185c31ffa3706d1cce9d525a00f1c7d4a2911754c42000000000000000000000000000000000000061111111254eeb25477b68fb85ed929f73a960582000000000000000000000000000037a088fb0295e0b68236fa1742c8d1ee86d682e86928ce4b32f27c2010addbdb7020a01310030aba22db3e46766fb7bc3ba666535d25dfd9df5f13d55632ec8638d01b").into(),
            hex!("02f901d30a8303cd348316e36084608dcd0e8302cde8945800249621da520adfdca16da20d8a5fc0f814d880b901640ddedd8400000000000000000000000000000000000000000000000000000000000000a000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000000120000000000000000000000000000000000000000000000000000000000002d9f4000000000000000000000000000000000000000000000000005d423c655aa00000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000eb22708b72cc00b04346eee1767c0e147f8db2d00000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000769127d620c000000000000000000000000000000000000000000000000000000000000000016692be0dfa2ce53a3d8c88ebcab639cf00c16197a717bc3ddeab46bbab181bbec001a0bdfb7260ed744771034511f4823380f16bb50427e1888f352c9c94d5d569e66da05cabb47cf62ed550d06af2f9555ff290f4b403fee7e32f67f19d3948db0dc1cb").into()
        ];
        let payload_attrs = L2PayloadAttributes {
            fee_recipient: address!("4200000000000000000000000000000000000011"),
            gas_limit: Some(30_000_000),
            timestamp: 1717713383,
            prev_randao: b256!("d8ecef54b9a072a935b297c177b54dbbd5ee9e0fd811a2b69de4b1f28656ad16"),
            withdrawals: Default::default(),
            parent_beacon_block_root: Some(b256!(
                "fa918fbee01a47f475d70995e78b4505bd8714962012720cab27f7e66ec4ea5b"
            )),
            transactions: raw_txs,
            no_tx_pool: false,
        };
        let produced_header = l2_block_executor.execute_payload(payload_attrs).unwrap().clone();

        assert_eq!(produced_header, expected_header);
        assert_eq!(
            l2_block_executor.state.database.parent_block_header().seal(),
            expected_header.hash_slow()
        );
    }

    #[test]
    fn test_l2_block_executor_big_block() {
        // Static for the execution of block #121065789 on OP mainnet.
        // https://optimistic.etherscan.io/block/121065789

        // Make a mock rollup config, with Ecotone activated at timestamp = 0.
        let rollup_config = RollupConfig {
            l2_chain_id: 10,
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            base_fee_params: OP_BASE_FEE_PARAMS,
            canyon_base_fee_params: Some(OP_CANYON_BASE_FEE_PARAMS),
            ..Default::default()
        };

        // Decode the headers.
        let raw_parent_header = hex!("f90245a00047e5d14e74fa24a08654b49795e57114475fd455689c71c5002f22a39e1be4a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a03a2d37a5619f9cbcb1828a0201e9185b67005131ed6236eac338cf759f0b9ad2a0fcaea4dfdc9c2ab4f0100b5c2fe33fba45c3ebb6a8beb0c156ccbbc901403040a0cde372a52b7bbd47e6fed509c5e43b74bd12a3119bc6a63c311bd00a80d524f5b90100000491006000000040040280000804040000000002010140000400000400004000020000880000200040001000400100000000500200200000102040040000000000000000000008110000080000000000008000004002001000000010004000040500020000000000200000010802000000000000020c010022201004484000000000040000000804800600020000004000000080200008400010010000000000000000000800040000001000000400000000408000000002000020020000007000000200000000000480020036000060002000000000180008008204000080000002082000880000080004000100000004000000080840020000400041040080840737513c8401c9c380830a11a98466627c3180a0c7acc30c856d749a81902d811e879e8dae5de2e022091aaa7eb4b586dcd3d052880000000000000000840395611ba056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a0a4414c4984ce7285b82bd9b21c642af30f0f648fb6f4929b67753e7345a06bab");
        let parent_header = Header::decode(&mut &raw_parent_header[..]).unwrap();
        let raw_expected_header = hex!("f90246a024c6416b9d3f0546dfa2d536403232d36cf91d5d38236655e2e580c1642fdbaca01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a01477b41c16571887dd0cfacd4972f67d98079cbaa4bf98244eacde4aef8d1ab7a043ab54ba630647289234e3e63861b49d99e839e78852450508d457e524eed43fa042351814b43a1a58a71fdff474360fbb9e510393764863cb04bde6fd4ca0367eb9010008408008c6000010581104c08c41068c8098020012402058d084a18a6408012213000000b02000000000102020800040202162c0424210820040e0405020215810c200800000000001a1000c480044002500011480822041080080c60e001840890a20850016240003012540010060c82006058024020014005480118000040a410c400000260800900000030004486a0820000c884400038060c08981201010322c060008200022100008195004cc082001049028d80000088000000000d402410030020080a102c2e00e1e141000044000208240045804001008018000800041110c1d0a4222056000201500806200190400049851890037500ac089c3000080840737513d8401c9c38084019fe25b8466627c3380a0c7acc30c856d749a81902d811e879e8dae5de2e022091aaa7eb4b586dcd3d05288000000000000000084039231b0a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a0a4414c4984ce7285b82bd9b21c642af30f0f648fb6f4929b67753e7345a06bab");
        let expected_header = Header::decode(&mut &raw_expected_header[..]).unwrap();

        // Initialize the block executor on block #121057302's post-state.
        let mut l2_block_executor = StatelessL2BlockExecutor::builder(&rollup_config)
            .with_parent_header(parent_header.seal_slow())
            .with_fetcher(TestdataTrieDBFetcher::new("block_121065789_exec"))
            .with_hinter(NoopTrieDBHinter)
            .with_precompile_overrides(NoPrecompileOverride)
            .build()
            .unwrap();

        let raw_txs = alloc::vec![
            hex!("7ef8f8a0dd829082801fa06ba178080ec514ae92ae90b5fd6799fcedc5a582a54f1358c094deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e2000000558000c5fc500000000000000050000000066627b9f000000000131be5400000000000000000000000000000000000000000000000000000001e05d6a160000000000000000000000000000000000000000000000000000000000000001dc97827f5090fcc3425f1f8a22ac4603b0b176a11997a423006eb61cf64d817a0000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985").into(),
            hex!("f8ac8301a40e841dcd6500830186a09494b008aa00579c1307b0ef2c499ad98a8ce58e5880b844a9059cbb0000000000000000000000004d2c13fb1201add53b822969231de6d1b0235f1e00000000000000000000000000000000000000000000000000000000049ac5a037a0c66b4526837e93e8d20bf5d378f060c5d4bbf5f6aee41be55598255083c1ff71a02fe196d9fcbd0980017d7d77c2c882c0851c0b06b59753bff54f8726e74870b9").into(),
            hex!("02f901720a8203a0839896808407270e00830213d394e592427a0aece92de3edee1f18e0157c0586156480b90104db3e219800000000000000000000000094b008aa00579c1307b0ef2c499ad98a8ce58e580000000000000000000000004b03afc91295ed778320c2824bad5eb5a1d852dd0000000000000000000000000000000000000000000000000000000000000bb8000000000000000000000000fcd04b8f8e8ad520e3c494b6b573f49ad4c9853d0000000000000000000000000000000000000000000000000000000066628a400000000000000000000000000000000000000000000067aa2de076064cbc00000000000000000000000000000000000000000000000000000000000003b20b800000000000000000000000000000000000000000000000000000000000000000c001a04c85fb7e9c041376918b4b3c2e520f0973a564cace5e49929f829236bcf45dcfa01a7d82fe70bf54633ffe3e69f238641695914c4e400c62de8f023b788b29bda0").into(),
            hex!("02f9016f0a018312dbe2840ab59b9c826fe99464812f1212f6276068a0726f4695a6637da3e4f880b901045b7d7482000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000a0000000000000000000000000000000000000000000000000000000000000004061653166346133333163313166333033623161626232376237393434633435616564326533653137333033366638626237633439353036613135333934393436000000000000000000000000000000000000000000000000000000000000004036616438623264376631333764363166393864613961313830316133353564383237303137666238663263656461343739333062613833353739616636646436c001a068b105ac4576560ec0e938d20b5b4cf001d161729597e7db17205238f4277629a059199bac24b6a7dfc08a3b3ab471d2052a64d367190b078fce60622fed4507b1").into(),
            hex!("02f91eb30a83039c68830d59468407381b7c830961bd94087000a300de7200382b55d40045000000e5d60e80b91e4482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000001400000000000000000000000000000000000000000000000000000000000000420000000000000000000000000000000000000000000000000000000000000070000000000000000000000000000000000000000000000000000000000000009e00000000000000000000000000000000000000000000000000000000000000cc00000000000000000000000000000000000000000000000000000000000000fa00000000000000000000000000000000000000000000000000000000000001280000000000000000000000000000000000000000000000000000000000000156000000000000000000000000000000000000000000000000000000000000018400000000000000000000000000000000000000000000000000000000000001b20000000000000000000000000f7bd944cd6d51a8b7dab54785608d8bda08f91550000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb0000000000000000000000000ed78ae2a0800d6395cf9e311659323bf2822c5600000000000000000000000000000000000000000000000029a74ba63dbfdb780000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000411506c3fea23c087ecd0605c103b3dc00eb380f2d4fa9a0d93c983074f53398ed1e558cd371b8704be8a1ddfd012027853bb1d2fb0599242ea4bd92ebe1b1d3c41c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000314c9258a47fcaf9a59a0eb2e02d8b503dab09840000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a7612020000000000000000000000007f5c764cbc14f9669b88837ca1490cca17c316070000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb00000000000000000000000090768aea66779f77f8e11f33ce35f3c0d0814617000000000000000000000000000000000000000000000000000000000366f270000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041ff3a97fcb63be58074d66c191d1a808ea49a1ec6a10f82fdfee5039f8c79425822362696b8a2792140275cd76e4df87c51b84e255390449fb00ac885706d3e4f1c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c7271b6dafb7adfccebc04a1c05160c11cc37860000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a7612020000000000000000000000007f5c764cbc14f9669b88837ca1490cca17c316070000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb00000000000000000000000023d640c90786e477226915473fe6fe354ecd24e30000000000000000000000000000000000000000000000000000000000d9a926000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041b555110518c88b56869792d167288b73f0b7a2789f0acc257208973d6f365d84266d67c697245198d433bf8802d56f9147264f8769b5f82f00e66fbb0bf55e701c00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006b3c6afc0a5cff7c98db387d3b9bea85fb2209660000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb000000000000000000000000bfde6228d74d163a4bd135395249348719d55b1300000000000000000000000000000000000000000000000029a2241af62c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041dc55a561f8ea7fab8b964c186e44b4d3c71fcfdda44cb37cc8c457ab53e081aa40a10c51841c5e8959525d82f08b115135763d3f2d51f9d6f928f040cec64dcb1c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000036db1e5d1a94acd2349a21db1c3c5699995f1110000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb0000000000000000000000005dff975994c962195f0d909be53a9c5d13c398c500000000000000000000000000000000000000000000000029a1814e46c1d000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041dbc3b402f947cad5e317951d6ba00ef6d1007f8617ee304dd251262dd949018b64b794f00c0a21af7eb3d769b4fbf6655057105b1a5c157dcaca92596808197c1b00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001f764262a8d96239275b6955f1b0c2e490e78dfd0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb0000000000000000000000008f0ac647326c6deb5b355fdd49dae8bdd496708e00000000000000000000000000000000000000000000000029a2241af62c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004168c74803581ceeee1233a6aa181da3ef1a015d7518333a9f50fd2d16ca2f1b30192406e7d61899c2533779b472d74cd686a3ff6fb903e60daf1abd7018f68b151b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000cbf74b5e15404cdc8d8391ffdbf2cd48971a20bd0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb000000000000000000000000a89182e4e2aacf3de24dc8c2b571714dab6508fd0000000000000000000000000000000000000000000000000001c7a82708500000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004151574458788fe9f96885b496f5621ff074a16b6c5e3de9f306ee2f88cbf163a95eea1bf4f79caf0771120d77b43093166e064edf80aec596909541aabf34cb551b000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000005fa18acc756d83ce08e0dd8db44b62b904190e30000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb0000000000000000000000000ed78ae2a0800d6395cf9e311659323bf2822c5600000000000000000000000000000000000000000000000029a2241af62c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041704047cfdb2bfe65bf969675594f0d2da7eaf951b4d57144af5351f17f8e9b81442eb2d58ac78c29331a1857cb270b5736ac65c7f450c2500ab423fc00d47f5e1c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000090a32be26009dfb0a36b3be2ec9f50a9fa0bf2230000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb00000000000000000000000055d8fd11958a4bfb4ea341142eb002f66c6ac6e80000000000000000000000000000000000000000000000000b7d5107b20f30000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000419fa97b385e50a1044ed62550e1e3f4960ca1e5f953c3c07458cef234151f7ec7191bcca4a46ac285fea9cbad1be1d86b4d11ac44584c292b2dfafc4e68d512301b0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000dea125ffc58cbfc448654b9a7697aca7501e5a2d0000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000002446a761202000000000000000000000000dc6ff44d5d932cbd77b52e5612ba0529dc6226f10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000000044a9059cbb0000000000000000000000001a466a1a1195d9496f5cff39b881225525769a3300000000000000000000000000000000000000000000000051766de63f8b000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004106404715a55eb449609a6b0f8720a68996a0072d894ed6fcfff3ff9d9590bb9561b43c3b1384619d46cbac842dae9309a91e636d03e67e8c6c9046dc00791a351c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c080a001cddc6970b581a8ddde9a548e83b23071bd2a59c3004d14fa513ce9d8b8860ba016e1c19a70c6f988873b62d499e9f4b827cee51874e1b8538be2bedd0b81215d").into(),
            hex!("02f8f20a8222468307a12085012a05f20083011170948c7c2c3362a42308bb5c368677ad321d11693b8180b88497998611000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000010000000000000000000000006168adf58e1ad446bad45c6275bef60ef4ffbab8be412ce3e5a692d0670703e8e56d648a33e80a6d4920166ba42d14ccd4eecda2c001a00e5583d7ac97afd3232bd3deed40c2c3c085b06d73e3ab0d5ceb77892219acc3a0491b2927fcbc3b82165428a5a3d532611792e19506db542d00c90b994782ae8f").into(),
            hex!("02f8f20a8215a28307a12085012a05f20083011170948c7c2c3362a42308bb5c368677ad321d11693b8180b88497998611000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000010000000000000000000000006168adf58e1ad446bad45c6275bef60ef4ffbab8be412ce3e5a692d0670703e8e56d648a33e80a6d4920166ba42d14ccd4eecda2c080a0be21db9df84d991bbc0d0b062553ab76be037059aff7fade639d536dfb0c03f1a04585c3272bbdce40e55637cddf1ef8797f07b875bdf7364f23526cbd87bf0872").into(),
            hex!("02f920340a83026c9d84039387008403938700834a420894087000a300de7200382b55d40045000000e5d60e80b91fc482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000001c000000000000000000000000000000000000000000000000000000000000003e0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000008200000000000000000000000000000000000000000000000000000000000000a400000000000000000000000000000000000000000000000000000000000000c600000000000000000000000000000000000000000000000000000000000000e8000000000000000000000000000000000000000000000000000000000000010a000000000000000000000000000000000000000000000000000000000000012c000000000000000000000000000000000000000000000000000000000000014e0000000000000000000000000000000000000000000000000000000000000170000000000000000000000000000000000000000000000000000000000000019200000000000000000000000000000000000000000000000000000000000001b400000000000000000000000000000000000000000000000000000000000001d600000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000a9aa45a7df41841e834bd0b1204989d7350aee2b2c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc7019c6b6fe6e4e18ffc69958a529c2b591dc978f1f1fac89108ebb460dd64903007706afa5c18f7e101f5757aa975a0bd584a75abf25c55721448a13a1d7280590e308fb0b2029032f838d15d12c0bc116f5f911f3c8a6fb992fa93e45fad91bf19a99eb9cacb5c76526d20f47ed7c6db5e32dd92c0b745ccb9bbc60f268a9ff82c53a0fb1ccb463a855fc6cf2fd322ce37f6aaf65096796b8a29540cf36d478d0e3baba70eac1d51f5518851a537a13978bb25a201790912e54a984562bed7151e6431b5623237b5f5c32cc6b1ed693ae159add9d421339d5db513b70e75d1861fb904a42b34762f03e4a515c63d0c25354f00a0cc89c9ab0d985aa4cf37069424604a41736425729dc3f234370a2ea89d701ec1f76934f566f7d479732e69a1000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000e115109f26205ceb8e28e227bc58a3cf500f8c312c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc71e7d290b46718f2eb8d8e47226b21529466aaa10cd18a76484854a78f735141d1e8111cab1c7cd5eb38dfc4aef92dfdb93a64426ddc0222fd5e4a30f989ffaeb0b5217bcb6d582638442705a158d57df4fe0dd4cae0fef3d7ad656d7c8f64aca10c16864fa9cbe4a423afaef0621a00c178e4b4e690472ffe82b76448ff2735d2a255341036d70a0df129fdbae5667947d4807b7e8128c1ce8344f71bb21510a2f63ece569a0d9aa4e77689965b08130141ba88f79e4770db8e6348e8fe03a3210ecced6257dee3d31fbfdde243940ab556f4740a9b5c20edc4125b2b843062f2a1bfe066ac56f7b6de17fb5816deb8c4345ddedf1260b6146d4104d8f3f8f200e9c26ca19f5ac327559d8a3c3ad2366014cd76432b74af62011c00bf564462a000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000999bba23dac348a9a7169664b1149abc0783d67c2c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc7177c60556fef8e41a8b3329838e6aba09df2f82fdb1eca33052df7e38516d68d256db0c7796da2b2fc508bcbc5f135afea7588df794a545eaed6e2a8c791b70b04df92e213d8956be5a9164606119114680a471eb091a3f8ba21b997d2b967bf2477689decfc4e8af5a2002725f656e731460120fa9795ec4a01517eb3dc5f8d138d5b1205eb28403be5c2ec1dae214c33b7e9837119da90ff45b858b23971b023148beaff65b4da6384a9354c3462896f10721015e66c28fc14ebb06d21d4af0dc09412a48d63ef25c92e7747d8142f2fa3e5368bee92360baeccfacbb49c9918f8266ca3596d83c00c23ea40db624ab0ee8e1b17e4d4744f5e45f19f18f45e0ce91a2f8105d523c35d89c4be394b0a64fec4a3ea0c5eafcee89e5b10fc9a5f000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000005f10f62a0d4bd4bb7c229aab031ae51c6ac31d542c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc7278fe9a6b6739901d9f2b36a9f4d9208744200ee37eae1905510be82e4d810a5098b7a0ae27ff42906e48d329011e42fa8f1640a183b6e0cf7467d7e429916c71ac1420326dac3098492a3e0244f381e68abedb76655a76d09f38ccc7bdae56c1bbf7de4b909aa89d6a4e5cb93cc5a201e287364d4d42d183dc278aa73f0e6201344ccaf60ba33412c9267ba2374741fa07d9438f6a30a87a551d71de792269816de70f0e23d5fc7e6f6561d813573ba8500990cadd54665619eb6f115ee1dbc1f9630be6dac5e1b5c37ea14f8082ae57f43fa3b2afbedd445134f15d62320782cbeed48f54ceda94c43de4f0e90e3ad9b37342e7b0a54105b65a045d00841ce2fabb7d092bba29ca228ae95f8ce51af403a152c4871d4dfc27166eb8f35269f000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000a30aa37128a2119ab89652914a30a956eae35f9a2c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc70b299790ba04716ba16479689195094d269b626c97827cbeedcbbb8c516efbd40e5c1120d26b239abf8d1db317c8ccf6d99ff9fda34bfab8d077bfbd6497e1fa2e2d657dd11880e5ff8e2fb7bfd3ce3a33b341fc3de41f32c05c87d0e7bab105139becd8882b7d141e1c84c3496287792d7613a544e08d32cd87c8dbd36272c426846de45ccb93788060a164a0cc2a91cfaa73d596b68633a7c8e885c47cd10e1e7c6a9d390f1d2b353e4cd439c464d05c4783d046777db2c35f122e1e1c028d20b9e2a27f1a4a23ac6688c1d31703d25ee805fe5d0df9d201cab8f87868deef149dae6283ba14246b2606dec37d0b6c16cd7bcaca1fed9d9613da85aec75f312a9834a01c99568f050a78c45097ecfb76145cf125cf0d2103f27634786891ca000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000007fa4b114d2d14b89dc6515399ae09fe2d7441b762c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc72a508522071abf0a3f074ed3ca628e4bcef7a1efe3db7ce2ea732c4f25a9f235293fcd7e88604cac4348079c347a4ec2cb0345660c2c3a7e79729450bb93483c185a64e9bcca1daec006367472d8b8ed5d429afb7c47d0a8d1252c7ea00809e329d02d5c8e4db2911db0693cb4c36a51d7e79f746d7582ba3cb27603c5394ba52e728b48ad38d788966430d1a62addb3faf3613b5e547ed4b71a1da0631b7e102cfaa1b8d91ecd1fab6eb138dd96b16b29b5b45d0a6208e1a9f2197c69f83be92558be565b70dcd0dac3eda3e02431e3a6c03bfd23a5b79efbd2cbd144cdcde129fd53821de617048263ca17984db238f09c0486f092214f10911e00b7a85c2205fff76cb75d2a35ed30daa89384c8b208d486f47ce93fe45f62c594e23971c3000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000a2f059c5e329560a7f4ae4472ee4097f015df1d42c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc715cfd4d9061fda04a7ff4c66cec0d2d482201b9f4a1117548d313ef0bf3e189300aec46572ae69d07eaf083cbde723b162622e228bfbbd8eed01d3c791650d3d1feed6991f37c930677e9838458b9ef5e9738a836bffa3c7a86ec6bf9e6f67772741d5cb21d1c97faf6e102f9c0bb15e32db77e82bc51b760c70a1f32979ce8a23f841593456260fa50974347c4d80710157b7577d02e16e771664a0ea8bc19512c5898cc28345b1822234a99f2145b1980e9ccfce18329a494fc596d1697aa7116cc2a22f1bf3db13f0039a69a1d0415671a6140ea4753473ef2c4429e6f39d1ba1bbad2945740fcd828cb427ce598127fe4a740e4a76c090a14b7a86341c4b0e812956c081103d37c667d654b881f1abe74f167c6bdfa656b388d7990ac25b000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000074c5d8389e56fc119ebc9e013cc1f7684d5a6b072c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc714a09e53d385d673c05228ddb26d7bc3be74f7a103f196567e2968287dab7a8b19aa753e75a75af73cce0bc21eaa8caae77d61af896d0159f6bd9d6c42d1a1251452e4f3b8058a3fe140472133d79b9bfffac66782355a9429255ea98c7a66b62c70a2a680fdce54eafba7e09739dc906327abcfb0199237dbfc28e9499b846216e3242c42fbb9848519d25efd11e8b30e873250186e0b2321d4af000f6e5e372c58d70a568f0d3e034e4aa5c1c8538c5ff07c1deccf3b779f371fde322d468c21d3295fdcadd60fe4ea1267c0792f7276b9f56429776e63c4f38b1ba75cd9c826c9904b4b750cd0a0d133969617fbda3046108e543d227a43d4f11d7662a0cd097a462190a390f639bb3c7df415150a25e7de70a8f58ee40650aa974cf16e3e000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000003a1917ef1b997b9ecaadea9fc836c40fc5a158612c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc71c711c12d07372deb6db123d4824f3ca1cccc452a89efa11b3fed5058d46263a28bb748862ad7b526192f76868aa2656248114f1fee8fc90b75fbbb0e399c131001f9c1d61b55c1479debc9cc5db22c2eb10a4141b952bf0e8ac51bf348fa0261fd0e8307a84500bf3a3511fcf55a04ad6bbf02e84c1602d1c8eab8269f5ca711819dd95e0eedb673fad82e15193673bd88573e883738c6384d6d7661dc2b3461407f83a9c96a0e61577c5f150ba93cb2309b88c77313db9580dababaf158a45085b16d533ad5b2d9d3f30b40eb653873f5f76e4d951c5a2dffb07b5b1831b22105c3325d94010e6a83488b7593452d8ab85f604b0d5c9349700b869215d4d922a84ffb448ddb0617857497f7a791fffbd68a0ca6536c5e6fe8e9df64e90f651000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000b319c3d1ef0f60c6e03085ce397aea2f2bbed99d2c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc7210c111ef03da72d83a899c671cf0d97a47205c0a462f206a5f8012870d75358247af48699c9c38cc36f280beef3e6b877a4ff3ddbb32881bded52bb7108a0df06f4fae4d32d4273f2beee224f1948dfc1730cea292baaa5391fe74f2a9cea702c013d1895ec1e8351f858074bfd57cd5c2bf13333d83df8d984ea144ae1650c2a8172d6f6523db18f407bbde01c23a218dc6280569b50b7b1ecd1ead768f82416bc1eda96770b4542e34e1dcf955ab3433af90036b78dfaf3d4513347301947026626c780884600afe5203fa9e8b15f6e3a72afcd4ebb9c068e57497e64c2920efef1c8b2187d434511572a803a2b8aaa35edd9658f0f10f681a10d251398dc149c1620acf0e50c8479680a199a24e0db6055e5c5227e780b60073eaefcb95c000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000f5da1b574a7675327d1b7339ad9570363c5c34ef2c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc728efef1c7cf6964407aa9cd0a21a2618b0b55d4e32e9713d47845be6a155712f164b5b65121455d7f647d5bc8314800fe4ce5179f1d1dfad006418f047020b36007b623ad50378124af98fab6274e64b1312650613ca19eac7eb89376971ec80280faca31d2fe889ae7a9ec4ae12f4fb8e7bc3b6e52110cc696397a4c289de6206bce497ce7b5fb9f23365ebebda365848486ad94136bce4c3e46810622c75b51cc40fc25555def1b8fda690461906dd3659a3e2450335b0af27a30c808158272934cb5d306306fb6b9e35677dacdd71fff28e8cbb85459d2a299b1eef4653ee176c207195dfe7c2660a74cebcbfe901bb9411c60fc50b78ccfb92c8036759851b136cd186aaa78f6f4c21a82f5445660f5175347a26b8a4523d4efb5f5f7fbd000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000009b0f286627130086f85f63177421ea1b2cd9f0112c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc715c82da09b1c96f513aae1a7f168c2f9fc77553cc19b4e0ec50f315d2f216c5d1fbcc5b43a01f2e4b53f4b6e7634599e9e516cde0f4b2d502893f7d66c843e2b2500e467e437a3265e203c19d92abbfcf1a9609c1b5b736cf249424afdf3f4d71f5d87add3acf628b1b8068b188277bf2d2aa87ddf10c8408079e40fb11468e011ec157bba5ecb886d5e05b05f225a5bc39f09e1e0a0bdbbd758c04ea304d02103ceddb427310d24f2f4e9f416f962e2c3efc58177fb7441150bd292896769b506a99097e337947e5a5019ffabbf2483d2867167111b3570088c1f4f5ba6b77b0798fca770007a70dada02aa9e6eb42d8c547faf5b53e75a8183bca5a40449a2209a2b54939c9ac7fcb528565001890ccf1c8f0d4fe7fe8c513eca3989a75b31000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000b5950dc266f30702248335d8f6bf4f8950b77ca22c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc72e937b4950068a3aacffe5a9841739e89c7f72706107b62109da41fc5e6a034113e6495faa3c3c637292220714a0ae733f54257ecaabe7bfa3658cfd10d33353089879446d6f6940e41786eae92d9862537549f1849a0ea652a3245f1e3c16080dfc882056e6e363bb516d39891fd50c297dcbdf20119721f80a77a57d15d51b29e4b31981b4bbeb71619bdd77eb7c74000573d6458a1ef3078ddd174138456e29bdd12d6b0531ee54f8c28d36fa55a17f818deee78441bdf5d2b1224687100a1221687757fe803d8ec441e2eec6a84abace83df3a8751f8b3456587182ac83b21c4730a474697036174b1277d3a1e8f68d424f46f72286c3871b4ed98a283a117d60c47aacd44384fd49ef3cc22d3d076d332783f8fe111f19ca5e5b92f4996000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000920c45b408f6a0c08c4a0481541acc5336e4beca2c539b84e65786fd42064fe72d5ba44e1434f5f3ed3b54cc15a1c1ed3f7d9bc70cfb4bd66c6dc2d7cd7c104893f0a9bf073fb3144f672bb9ed1eb2dee72e39d20bbb1ba1a071f453a92bc1524762e9f7a6d4ffb918d56a059165bc2df45364212fb7c74a2cd0a4acc230c9875eb343747844b8abf20d0e6d5ba61351d73fa7b41de3d6d85f4ca9e199bffec3360f89955df46dd9ba41e23b7c7478cf3d31fe0707dbd1c40a18e480905899ff851dd2a38405c8374b362f64012192a6b2caa3db07611d351c33da2e7b9d5dbc9880a8ed60eaced742390f5d97a1dcd4b87b6ad02d1b929053e54ba0a221038c7ecada4afb36205305a7750fdf978b0ba49904fa0ee6e8fe3627a001814f99e01c80549a424ec106187ac0bf6331065bee11b7641b6f8570d85a3f531205dc5fb5336a1c8bfc708a2cf6b9378e7cc6f8e075e76100000000000000000000000000000000000000000000000000000000c001a0537050b3611333491c79373e0026e1ecf8582a6cf5d82a9318b4ba9b1c82a5b5a053127290f0a4f4a3dcf8cfcbf1a874471b0eefa1ac8cf0e3b4ac97f58f5069bf").into(),
            hex!("02f920340a830269c2840393870084039387008349cf0194087000a300de7200382b55d40045000000e5d60e80b91fc482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000001c000000000000000000000000000000000000000000000000000000000000003e0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000008200000000000000000000000000000000000000000000000000000000000000a400000000000000000000000000000000000000000000000000000000000000c600000000000000000000000000000000000000000000000000000000000000e8000000000000000000000000000000000000000000000000000000000000010a000000000000000000000000000000000000000000000000000000000000012c000000000000000000000000000000000000000000000000000000000000014e0000000000000000000000000000000000000000000000000000000000000170000000000000000000000000000000000000000000000000000000000000019200000000000000000000000000000000000000000000000000000000000001b400000000000000000000000000000000000000000000000000000000000001d600000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000ff5b81c06a347cb8268210e970cfb650fa76137528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae629a1fe61acc415f47fb1df4678c0aafb8977b62aef6325be11f3c26eb36ed5a825acd8d663439a51e60752dd4511d9af15c89ef116130f74c7f10eca596ecbec245b30414b0c4b95e72d6094ce22cfd7ff21ab7de7ec918182b0c1c91ae715fd007ccc9e09775420c6e7f1bc73aa8c88520e37d1428b9f792e546395c853f6d5090e477ddedbe861b0841ae47c1edd5619cc919f03fb26cbe2e09183dfaafa7d24b2318f73d8f1426223e5a1380042f598b2ad19caad855685f696c40bc269df1e7de6b1c189beb2603b86d58392a12e4891b3fe197f318da04453bd335049450f38465dc2a05ea81d5032e670331822eeb3d92960885367ea5c913eab351f7710fb70c8c532d5b2257f13dca19e0eac3d506133738244e5db782a73e22b09c9000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000242ab389dea6b46da7fcfba55d6318c1e2fe7d0628942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae624427bf3c4fb33035faa23446dde42edf4b00f2c76d2284e2d589694ca6896961a078f3488fe8ba820e217d4477912fd96b0ceae34a0608b5db519e302f53c712625caca9ac53703531f7ec7572b9b5f80de7160a5df81029bb29f834de0c3740b15818ff91d30b341b30ec6eed99e4e5bc4aed0101c72e092babbe7ca1aabfb0df5294675220069521b1ea0315edc860450086ea1fab8aeae7bbaa84189b84b21af835d67bbd00b61e7c2fc9b3cddd79f927801d369c275b6be3112583457b42056e57c9f86eaf48c30534ee59bbbff72c77368ed4d3be941f5be3bf9f474df07ebcc794c92f409a4bf625adc56b6076100e4c8a04a98e7b7f8d0ed3e04f13f0e9ba6c313ee27196f25ead7314f56d063e7344e9bcbb69cec65d8c5be59b426000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000056c9aee5056ef57250fb974ff9bcab45b121dbdb28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6284b84661b3e7d8247b71e03458b1560b0590094f5e7c7d6b0c340a7a73d950126b8d6826406705cca25312888531e3595769bcb60c783d4699c2c94fbfb88451d9d8daffa3707389dc7b6d951a9e7a3cd0fca4754b570a834634bede4401c3a1858e38e1a0174d26a4134dd4a9ade06b393269fb96421ed6377282a86e3d25601204f8680e2e112771469071db800cff8738193077cf8f5606402056a8d701d116fd6ce2be79eec293fd6aaee23f7b7ad1b500f01025a5439e0caeec5a4c1941b84a0801f23cb5690a38495a4810d6fadaf2ae74525bb9ee60110ccabe1470816b5b742d4b9757a21846410f848aa6acd2ba7b8ffaafcc459f3f465c9a5e5991f03daf0ef1a0bbf2bb8fa6af00517e4e4ffb0883e51531d25a7a5fbe74797f0000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000e426cebd1060d4bdd9051feec88be9403486cd8028942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61bc3b3a4f2d260a159b623c703b914ea585c4ed2c73484dc1fb362fe5ca8918a0f2b415093c671280a1a9666a4171305736ba04b4c59979668de99eb6207c1aa0ab3e2931eb4d8128bf58ca6b305adb61b8296f15415d1edec109f8757f8644b09bed0a4184379b56cf64300dbf1b79070e76b62cf6f30f3aafdfd2980abcc1a0830d97b570b40c69d246f6690be9a1636350e4da7de3976ca6db1bf61006beb11e407e1049d700a15a3e10cf0e7e12c374c872b7f8a0772d9b528905fd49f2e08cb7dff6b3c7615409135fea859c455400f2471eef275bdedd852ce81f74be9013d4d176587f44cb22e142cfba09332748617bd8df8512e88c9e67b90b2f1fe154f456dc600560a0b214c09e28e7f7848eca56b40269de0e8b4721249a221c0000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000007c86c46c5cdb785dc78ba1837e0225e59450b18128942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae602565c1759ddd04a0819a7aa7a96edc3cd222f119d4e6f550d4bda9997ca45c118468ce7b1c6616208a9f0c5857e2bbb96ac51c73a58e0e1d60e1e2adc9ebaf1160ade7456bc61891c670b5b2b6d40ecabcccb3086f4d79e2147b8d4fe889e56169db8287264aed78a3bcc7f14c97501cefeb5ee0fdd6f199a32daa028b8df8b2b982a923d639cec85d15255ed9b91bd19e26dec3da5fe220677f0ad66bc71a30bd2fe84c4ff72057115eeb8fd51c847682b147d429739025cfdf6e26b7ae86a1d48d009ad19300f056540fdfe08de659ac2f6d4f192e3b1bdf6ffb41576cd2b0753ba92215508561aad8c79aa9c34fa5a3506e7ec7fe7645c810c410c471fd12526160d102ff8a08611cfcfa6dadef243c0513f6739550b9ffc9ce99fdb7297000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000641bed5addd2a358468adf163e6c6ebb80b675da28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60ae86a5c0f75cec4ec7b453be4fdfc134090e99d66bb9390b908782ad550f426039fde3482e025803dcef8fbe6923b6b6af7a046ec975515430d24443aa15c5c2dd3e8093f9cb080600349e51d7e56b5fc6f0ac083da6d63e03e46003a3a379812c6e738e5d34d372a656dd19e8410b8a00aef4cf3a26bb7076f709953bfbdd202ce2b1ffedeba392f9df3b208b6ed2becc3e6d4b3c5593dc93c5a9d8681d1c42fa7327bc880d13029a9213f88529f089da75c30ca2dd52d0370132a03e378f3006a83b97ed66aa961836459b7d4f6544f1550419d493fd534bfe21266a60bae178d3a497e3e9b0d285015e918a625658ca52d21d6a2aa62667fc4d47039a1900e76a537453b72f8adfaf853e04e68c4f27e34eb84623bdc0b3e1be3648eea2f000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000dbea52199557b8fa35a0fd208d7ca2b964c8955e28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62600f9363c032d201bd8616f84cabc6010919aecce646109b2f807733be53ba8142a9e34992574b9a4a7ece6de4e9ec8fe60b42fa48cb216c242d9ccb49ed4851704c306d9570788730c05ce290d6b5603d3e51c332a9f60c90fdb8cee47076e1ba1bb35ef430c23f2e6100a541864a83e63e3ca07b054a1ffa1283550fca44700e5456abfa56f15552481280c5239d2f3d3822d1a83abe0a973ac3ea2c9647f25b22b68aefc0243277645d2dbb062595c5b0c26aa18a1449a4d25b5d0e1fcfd26dd9c789797d0941377dc1516dd7d4d1912b74636d7a8ec5fbb183ac34d841f0ccb72ffa06c254085208b3554bacf41f623c7d051bd72d814968379f9f53a982292f9b8a2ec73d2b4f33d009b871003c205d71063ef6bc2b810d0e58906322b000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000339cac126a7b5d6807780f9ebc3b5f0a7cb2626b28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62f3d9e4e7ca68fbe0889f086d9e238b256d57d515ffbd3cd40bf0aca6b0492ec2dc950c2b7d6c026158d109af1da9c32a47380fc0c7bdcd79d61c43bde7ec5fa099f3b2158ef7935587d1428124be6d2777e283e2dc385eb21c037efc7504d550b98049a7454f9b5d51b96179d8b08d74c65ee27e6efa169f96539abcd448d74150f3d44585790d7af41c0f47025c38f93b3df962d53aba203b2977e0b03a86e13fb21ca28b90c530ff14213e64dbc26c64f4fec89ff0f5e8e70bf32563fd5a1213824e8073aea4908b30f0f2897305725b00fece62de3fc86ae10fcb5158cf42fc550c1d59f63ae72212609bb35db10080ccf9bf4298d993b3fc4da9481d8c12aa75eca7e4c8744599f89ebff87dcc5b6c92786c4831cdb01731ad7492eb2bc000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000898cdb69ab8946814b6b53481d2eeb5dfae589e928942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae628df4363d09f14cc0b180122fb355e58aae4f248a95537baaece2b465bc4b39424d6d520537eafb05785e691f7c8e3a68b00f3e1a71059f763649b0fa1666ca5250cd5c227f0a746063f3dc00fddf2e508f4f005ef02cf0f740addc832f6b71e281b74883a14686f41b30c315c0a7ae5c61ad87c6bcd4c8c2b629878383bbd00278c74f9c3fb9829720af7dc49292b1b74ccb19e4200478551d10de8bf5353ed1dfdbeebfc985888567badf136b727684bceeb41f72138ffd953f9159506f64d0af450ab1db15d160f2907b0c6beef8cb85eed914b58747be81f7ce5267f14f005987b9b30e3a076a21edc79868c2a54075f918b7660435773140c42100b383114ab9a984050afc9dabbd36989204903a234ef647f51501d202267511bc64db3000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000089bf43f6d3014039263b05bb7a01c8646350e6328942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61b5099c5aaa2aa4fe3ea537c82fc999c56d5e1a0504d48316162fb007247a9351deea520dac62b7a7ad005b3abfd18dcbdf30ee5f9783c58f0ec80cbf67aa526172f62057d065d382ea631782dca3668a56139d25322e8d3128b8ba8989e5e8807e95e4c13049d69ebd673a9b277fea16879eaf3916c0eb384ecb6882f3bfed32a056a0673318cb2dd91fa8cced0076fab50f1a48c9f0cc85564ab58f4280e091018d09d4176019296ec1bead6fe3ec0ae4f653ecd1fc79fa5e76c6444b57a4f114d78fa109f07db217968af2f14c34de42809f4200c7009896107a01edb36ed008d052f0f106b6f446ef5fbe229ad7274519f6bc3208d2c1742d448595061f40f8e73042ce05d9cd4dcc4e753a38b497914719f634abff53e5105b768f6a968000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000006c0ba81f432a713774283f812bef31d90d20135328942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6188e2c1a9a4653e70d1ae03b2750f721a28414019aaa3ca024ca734b2e2d51a7089a71e0d1ca62857286e070825a63ff61c2a5e19b12e62b7c7353ec13c2c8a60d7f667f6bf650bffb6548aacfc54a3c76b2291a2167176c306c5b9933192ad02e9a48800575f56460b02e120de938f514ef627d8b3a6f52e757b3fbc9c7e20f1960c8fc64330e626936c22ad350096fdfd0347652428b8f8815471d3354e31f2ff82e557609b86e761ec1efc4a25dc82961a29400cb728a6d6877394439f7e42fc90e0e1f03b0b56cef81dc69800ca3ae06209cd4340d069234af11e012b28a2dfe3dcc6612173df72ccfdeb30e65b16cd72d9d93abd804f177ec37c0a225321a27158c9162a130048d54fb3ac8a09e9eed1c943f5b030cd7e225a0ef43d436000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000093e8b4942d760c8131989a31ddabe994224fc7328942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62aa72c25afdc2c65ccac73fa14504c772f86671d5d24ffda4c29c80049b10be603efe26951efc3e66b1cada240b46ba47c193fde7a505eb5f7b90ee821c492871a21e84f29903b895ba690af55a5c6e4eec9c4993f2d6fcb3bdce49c2272cc7d0dd2a2208ec72027f259c47be09ef2a5f91c5f88c8f1004d2f473ab5d591d86308f27e6a91d1a5f4c22d91e5626aac68f4ae8fc70627fa608b37e6008b3ef4362745e5efcfc745c520d13870f9b3ddf41372c76e05c8f8bb818b9fe0b4e7e30e1c8b1ccdec4317ec340fcc5ffe195d2e7adbc6d518d0418159c277887f87ef7512daf179dcba4ebd96b8e46c74c975fb1f74fd2371f13abdeecb82d95397c7d214f6fa20a20fd820e80f89df05fd6eb28bc38af199f97da922116b1b81d5992b000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000005694d5bca63987fafbc506499abf1d3df7542f3d28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62197da3e2e450b7e4295785aa9b2141cbfefa00bd32d01e38bd3ad66cbb4f9bd2ba627efc5a62548611a3b206fecf82937d0681a957bc4b05b69ebeb4b1aad14068983c83a07b558cf74cb7ad93f1e08a10bc1d31938e43948f50478ec0d5adf1c61551b324e6af3d9d04329c7d7150e59099f59c02260243e3ba7fe9bf7a9670ab1bffa724e9e9e13e378c12da3102adce093be3cff880e62c56ba3c2082e4111f51f6ddf705a68987e1d52ef54285e1cc405a3f9e1757a0bacafbb11a13fe90d0e40768ba2e30f81a9b67154c47e0c1724f63bd199962a46c566165abc73c729b6a6181730a69348c8ad17c511756cdbd60a2c9f86f2392282f87fe5b37ae60b252be50c7c03d11da135928a6eeb34feb8c52f23d54994783bb1521a94dc1c000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000d433b30092565fee0f5571c7b4b219808cbd525028942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60b645f78e9ef37499005d2361154b0ccbbf8ba935d20fa8d90025125262881782c209330e318f86aeb8bec2351bb0d803c445030eca84323234e2cff7c2e30bb12f4886b1789a434d736a17ed9d51f56848ebf962dd0c3c517d68a7eb110fe8111dd1723aa0567a2c1f5fa1cbe681bd82733fc402554d09c55ec7eb1f17ce93117a196deeb54728ef1aeae66250dc819df5e101b22527bed75aadeb3dedc6e0b03473037f988a0498c180c24f03eacd75c83a2ed0f18ea79855d46695e6ea15519ac138f865c649909fafc068e7b3ca65c06af530e3c38b521a2ee2a19e43b121b12033ce74d820c048886b14391417046e03f3686acde01ff8b76cc33d818711dd6811711439bb2bcc43e0a5e43682a1f8870254bb24735bd658d99e4cdbbca00000000000000000000000000000000000000000000000000000000c001a00d851236f0e494fca892608f43aeffee730c92b0c1a9871838a18cd14f80cd47a00f6027f2445ee8452f573d074a0f0879b275eba8c0dcf2c77b0f874c68a1d01e").into(),
            hex!("02f920340a830269c3840393870084039387008349cf4394087000a300de7200382b55d40045000000e5d60e80b91fc482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000001c000000000000000000000000000000000000000000000000000000000000003e0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000008200000000000000000000000000000000000000000000000000000000000000a400000000000000000000000000000000000000000000000000000000000000c600000000000000000000000000000000000000000000000000000000000000e8000000000000000000000000000000000000000000000000000000000000010a000000000000000000000000000000000000000000000000000000000000012c000000000000000000000000000000000000000000000000000000000000014e0000000000000000000000000000000000000000000000000000000000000170000000000000000000000000000000000000000000000000000000000000019200000000000000000000000000000000000000000000000000000000000001b400000000000000000000000000000000000000000000000000000000000001d600000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000002fc452d648f7e3043f6f3c7e05f79824f07e126528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60d070771ea1782f53e75601ec9fdcbfa8165dd10e78facc01734960b460e8bc002d540e4fc9470713a92a102ba6efb53f5195ec39df5fa7c39ded840cac2665e150caf268538195cc7155c61877d76b96f4500268a5b291d0fbec018782434231b0b1b2421890453a77b437398b581794d0e9fcb7665677c1baa6bb4fd32027e1eeed729491ebfacb2d025e3b6fa9a4e9ec09fbfa76eda164907561e7658678115a3cd07f44dc2b38d4a56cefb1b203a78d69c07e4b793aaba302815e282b4550bbb96dae434f7e75c0e9ec7e3c13a7c275a63774c9a1afcf71b5d601d33d0fd150484555347b520362d8ad08e0196ba5ec3a86867e1045fef7c06cd764dab050e2af04a61253746c3b43cb0fb746624ed90bfa5681bd43f461dba788b679367000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000004ff1a4ad03dcf3e228712b54a499b6fecb56116528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61cb78c9bf2c47a0e409d66d6e535bf475722669c70b0b38b8e6101af2722ee2e02cdeb14b93671e5cf75d02fe89b3581ae8a6754b0b654661181011545a7f8422b5edf9d0e6b47e7024f39de0eaeae3dbf929c8331b7e355a33ccba47abe2f9701dd64e2acf93cea4139b852b2f5705045b4f8dd5048bf6773b07c06cb999b8221680a8796febfc0521bb58645c71e60f51be506241a3245363ac506b023c4fc2847dd48ed1b0e15f9a15c94a96bfa332a39da0199f1ac0ac9608f82bf01ba9628b1af262d218d7e836641ef05c35d09411785f61d9b1fc4ab6f7e673802742813e27335a6e6332a36864ec19142c577acb77e3843cae4c3a753b324227884d81c9260ae6dc694e4176e913c26fc558763cad7f60bf123006d3d72736fcf3ffc000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000012d2142a7cbebdcc7ff6cd5b95469abbe4cbf24928942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae615880e10593596d4cb658f00c5562bc58c5a6afc0eb2201f862350792f1edb7512169e7be24ecfce258ae13fb0a4c8de4c8fda790563f7f3ce774220cfe972e1268cc85bf6345d455dd833eb8fe1c980326cea7ed702049d4c86accfe458012b1a0053fbf513220999498d04bc78ae54fce71139e54d9d5746713eeecfe3cba305882c63745df15d918b36cafd8f50cf7216e90b1aa8bc9dc2e3e5429a3bba641b17ac760debdb5700c4c9bad98ee427f265aabf94f2622f90be0ab8fb849ba716477b7071a7d42828716a27e9f6207c49e37bec930d029611c2b98fbd78ec62247bc890c170f59ec6dd157e7975901ed9142563c3d331af055f49a7afd3adca0f8413abe7a9d392410e6ec6193dff28dcde681477d34ea9def60978cd57c7dc000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000e6da5018bd56d59ed86c34bd37e4d3c6eec61c4528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62cb04ba4e26d335daf21194aef490805e7c3e3e0410b593f7d7459533908346917b5c793a295e007de396925820eee2c04d47d4027546318716fe4b2b6f323702cf3eb9676205471b0c77116b85d9c5974036a46290df0a80bf86d609287f4a123339d2f5621ccdbed149c2302dd01a3760920a61e8b6bf96f0986b20ef8865224b2c8d18e6d7d92b199564612d8cb33b22423af4c67fc793e2fd504504d51382fff82ab032b15d672bd52ce82c75558fbb1efa9ba7bdd84b295a781a00b04541e312bc1d73e8635d297c7f09448862f568ff6062b5fbe6f48bfb0f4bf3d488f1ef3ade38a1c0f9e21d011193e16f57211ff291da3cb0ce20ef75bb1b426e676267fa0f47ced1401e91a26abf637b88447b2794df9b56fa60a62f51ba414e9af000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000dc5bfc07d7ab9392b706b0ea472bd659dbed8f8f28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6217cb74019121e68260c8908a6df82d1d728fe3287602b826397b023fc5e43302f245a4ac4b7c1afd8f1fa9deb074b8535cd4e3bd2d275f28a3a66100810b4890ed44be420c6b1409e0e33cacd54bcb47911839f76905b0c6101d9b2b1286b6723fe89341298544f07b77c3b23c16b51114b8f438f9cab043f06122202d2801227cc6e2ac7598adfca6426dcdff07e8c09ef8299cc6d40a7edbad876ba25b4af1eb1ebd3f7cb41dcb78d8972fe5c81bad17a9e98bc27e2f2ea9a6a183002fd362d28cfb0bf8ae7c4536c4e22623f8ed88861561379138247db9d18bb3d683c272962c3f94f3d2c6a4206975d1fb3bf9eb5ce122ec5024fe2f97ef03274350a0e1bdaee3a1550eb966610d56ab224fac4fa3a8d8a2608cda75d2e02c57e2ccb21000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000005b4d0d491e04171fa86465244e305591b539af8628942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60ee7bebe2389775c6f21811d85d1e3debfe450264c80ee39b9d17cf7a7964d73198cd9411360e7d18ba9766bad00b081e874286654ffce04106cd07ab39a80f31fbab5aee54c5f0a51f72b785e8d8be68f4c3ff9b72b7ee06f06fae8257ea005167bff7a7352371616ce294502328d16cb4669bc35f8a6cd3445de9db7beb3fb1a59b7744a0d611e6048e085144682db83231fe279b77ae157193c4f341e1bc32b4be703dd580cb27c297a8682553f9edfba9fde365ad5c5cdabc024b5bb1d6a2845eebf6142cd01024f1dfcc061f25cd668693f0ff709b802c5863363ec08ab1b95f0c7daff4283c94e7629c22514e02442ef02efe22ff99548924f28071ad60379102d4becf6362773c6159a4c3a93a4400d21e1ec39099e6ee9c86fff66ca000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000667c943726579c6b8b3ff2cbb47126e52f59f80f28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62c940a714aae7708e5a0513ae7fc00273f221598ccdf6a9125a131ab9d8478850047378127753b3cb5fee3a06630428036b23cb039525fecb0de048f6a2eff1e2bb4d3dccafe9dea03935572b49c75342c8f2f4227eb764a76c5eba46da3e08d261edbdea002fbbaae124dfb642481bd3f9c38367c5366d46b860e2dadb3a08103410883fdc095c9866e8a0991556813f661fa4a2c381ce33e17c7e817f0a87d17831194f810d395ba0cf19dc742f1cc37e80348d8450660c00133a6af1703900ad346eb5f46d2b8aa146eb19ccb875a7b1cdcec1718e7111e1343868908bcf003bd5a001b9a8acf992d4aff869e306241ad07f0834dff041cd80e5ad5f842c41a2753564b701f03089b903befd41840d5f3cff2e9c7975bfab8249fd20cab46000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000098d88afdfb242127432c715189a775f12bb3390f28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60efea4f327aefa5e9ead161dd6611ab61df4c9dab9c5988d97612cc479a9f123016930525c1781976da3a14df82768f937f7f70bf96e3cc26bb5dc21dd6cd74f0a47c7bdcc19947fd3ec249b699347c0d78cb35ed6ee393354289f75b924b388227dce1983515cb1368327275a37996e7de1ce67991f773077fb86c352b7695f1d4cae6a343eda07d8b2897b6ba428ee50208001947d75c0d3f6f4730361878a164af0f33cae3197eeb8a07523557a9391a05a0698ad5ac0a50d65dce5caf9ff2bb1c4d9461350a8bc0f778b1a009f42949b7baa3bb2ac472ca3e0f47a541ede14f6abf64d02842c87ad07403aa75bc400b93f94659a6a3e7d5cb45894c0b5662bdb9b7cacf927aa1d2ccbf2307359dfe2dd9853858fbc2309cbd84222ee1c5a000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000082fb30bc5c250e9d57cb16e112a7c15a984f94f728942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae613503f6c7ddd1ade21af1b97a7ddb381910c4ccd550bad7eb9d4c773c0d1f41328becb7e3fea7c0af3e8c3bbdb39e2ea67650c0fe58804bb439a70917517e5ff27dba88c9e32b65474be306bdd1ea2b20b4b30d1cf2afd1b63cce6ccdd36aea8089ce6f5703c70ece7ffc1991e6ee367df5b1357d1dd9589ce9838d41972d5f816a37135c21e77f563b25b36932bcb690a61e8d19558b52872e0f2b6de93b686211c9a2aa97c567fd45b927bf3cd9445592d7b0383c16d2d34639556b23e59d5151f0bebe67a27cd7b0aafca46bc5c5dbff099374004d90679a52535a5e2ca7d27fe43171a04b24a499318fe7c030f7985931c7800b5fa5b59687cc52301a7bf2e707160a3e4325baf87bf59e2d8e833668bca2ec7ebf3353b36a0e26ae4587f000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000fc3d84fe66f8f6ce3496fa925f067884c1d11bca28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae620e7d1baa83df3ff39ace24a671d6b3f69bfd8f8e38f9d4c76394e6127113ef61f12276b126303fc35cfc4ea0c935c8d1dee00d9cb654e86b2a2ddbd8b84ebc91169a19887203f9066caecb932178461017d06f66a4ca97dc7919838228fdd080de03a06d07484f6f0840471305fe63e3f60c5682a778b3f180643bc661eff5f15aa8796a3d83ad29685a5743838adc982403daf3bd40c06b7bd525d60c641e30b56e652955d1bc7645537d3b4fd83a780210236ec879b2a5595033c28b5b8ab260deb20e2d5741eab2b68caf50f1192825cba2d2a23ff349b266b5a3cbb93ae1d3b6acfef2820bcd768afb6624ae3a6638b757f447fe0ac3b3a44f060cb92cf2ff522eda5f60551b816e746404aabdadc902cdb01f9c496b7c145c6fcebaecc000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000003911c90a4fec5fb6ff39ae0d502dda70c032143228942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae614d76f34519b08809f7d882290658a65b920507b47a4f1931ab4ed831deaac542823eaf1391da29dc538f8ce3fbd4c67f4bfb7b124d8feab2a2ef58d37fb5d72172bbad25a35eaad915bcbc4d9ef1c8caa54694d61524640fff87406f056d890255cbe5c7c56332c1ada63328614d7831c79dfd56784563280da99982420d63301abe49846092d86265d808a363307466e3e15a2aef7e2168e1df212f5b4b7ce27ba044096f19795deb46c94c116327e72e88999aab61150ea629d14bbaa75d922efa2a4ae3c8dd9b7b2306be9f3f04cd660a13c2631d321cdd0098fc88fc6d8070fd6df8a8e2b4cfa9f1a7af64893f2a9759c62498b592d72e36b60b70cd7d21a6c3c5dd10dbb80507845d384e2a0a72c47d82dd9a0eeb7ad8968623c61b40d000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000033687810343cee7bdea2cacca78ea6c540c7c3d628942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61166c0d2307a5ac1c2c7fd55e2263089047b5621a3ea51a6a0a3a861ea096fff1e8085f3ac492c9b35a72ee206e5edb6b1eee04f97198759ea72a902cb1429d1261265b59197fdb6442ad18783cde7daffd682166a58ff620802a244dc224ee619b566dfddcd48572a8eba16b2b0bddf00c812c88dbee6dae0d133004cb757360d94bfba6eed1593868ebe4784ecc3e38808d8771dcb061234fe503ccbae4a1c1dfa73cebcce6e299c431902dea0fecab696c7f2bc74e9b693f724e61c6c68e72c5b6d36705d519d7bdf166e45bfdfdc55728bd7a1e273cb9b7e445e0d6eb7f802b136c3ec82d9e2c31b62669d60dfed1dcb953ea9c53012a2554281dd3a882704e627b208bd0c06874eea409ac1bfe30bb5a3afca1209e4d4d0d9fcba61c864000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000224a08c35cc1ebbfca7d4b4f4c3210b1768c48fb28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae616b0b9916fbd613c79327220b58a6ed1601eb6190ad9984011779d4f26ed0e0b02b9cdf0a697390b1dba01fbf8839aedac9c1d1bed91c2f62790feba4bae20a7227f582865c4e244144caa2de39629356aa3b727db714fa23fa8eef1a0851eaa097aef2ef723bdc57770ef7ba686ebcd49afaad4af272f5b56936ba2f324f5f3143d38c2b9b3d1ca2cb6b2fafad02c3568bef51d41f74056f61698e25ee1dce7048d71b1afbb73453538b6880c65d6a3035c90725491cae13aadf6f7a4c3389c0f8607ffbcbe8681ecfdd75aafab65e190a96fc7084eda9f76a9b824659f60ad0654a033acf4d47c021824138c94d55043f6c062ec30dec32d8392c9fbb7d9110ac815952d896e580ea58c0d1255e9abe5e21c4af2417992eec652ff131bc93a000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000412bd8f25473f2fd93c05013980d2a82d3ecb1a028942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60792158ffb93c36ee6385bde89e6278d9c81d0695cf7fdb4c161be1d14b9879c00d67c1adbefbfc9cce1eb8a76db8cc0431266d9de688841b796a893cfdf604f1717df5e42292c9036dee88cce2e1f72f123cb38a5087cc69dd3082f72c2b10f278a96ba70aa94dddf347c1147f0746db1e19c2ade2ec6ba03471eabb7569ae62c96515427237b01d7269324b02adc857a7d2a39f25d1210430b7e00b07f05e024c43f76223a90b001631a2c3ed5a904b9106f97374bc39addfc5fec805546b3049966a90b90356e0b949176e1bd76493af2d5646374b9c8864fd1da3c0c8b0008d3e477c6bd287ceef5497bffa552531f6914362ff000409ac01b3125e07f312d035a245808afcee7785031f40587cf25de1dadf7c7255709df7636c7f1f76600000000000000000000000000000000000000000000000000000000c080a0b51e7f43ced124a0110c23d55d5eb2a4b094af4f5d366d955076ef471110ca3aa06c7f8df564f4090b81e8bcaf17637b00ddc6ac0513e8b71522abe47999827380").into(),
            hex!("02f920340a8306969a84039387008403938700834af50394087000a300de7200382b55d40045000000e5d60e80b91fc482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000001c000000000000000000000000000000000000000000000000000000000000003e0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000008200000000000000000000000000000000000000000000000000000000000000a400000000000000000000000000000000000000000000000000000000000000c600000000000000000000000000000000000000000000000000000000000000e8000000000000000000000000000000000000000000000000000000000000010a000000000000000000000000000000000000000000000000000000000000012c000000000000000000000000000000000000000000000000000000000000014e0000000000000000000000000000000000000000000000000000000000000170000000000000000000000000000000000000000000000000000000000000019200000000000000000000000000000000000000000000000000000000000001b400000000000000000000000000000000000000000000000000000000000001d600000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000091bf80d5452bd35a48d6f8c86c16973b9fc839528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae615fd0bcc6b78ca0d068409c82bd6a0560422aa4e76a39747cb3527779b3d10b1109e5e3b7e07ac635de04e45b49baa610d7d58dbb5334a17034fa72e42cf97951dd1f25cdd214841af8c9cd3d7fc7cb6f0a63f896196d8593e0f950b652474c912a16a926ddf0fef1ece209525ebd672fbf331eeac58362355a6929bac7aef89020264f4a382f15cf0e86d637ef7b6525d039c5a26c25217a65144a8fc0e8be91237a63ece99d760b22c098fc06ae96b8221683bf9bd8738e532602c457b0f8206dda8bc97efe551ce4dba6a7bbc3cd0615ea954656fbfaf134aeba1adc149fa2c3415a732bf747cc8b4efe1b7aaa2c9c17d3a6e0d0b8a2732716648bae7b98d2c03d6480453e3234429df386a14b0a5538ee41c2cfe083bf4716516c4d36e77000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000a88c2b0fd676117d9825602c10a26874eb53462728942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae621597357b85ebd21923a956cdadfac2e29949d8445536cb2357a98ce389c9dd00613458c19d646984ad2b63f2e6bc587943f2fda4f96d23391e0d375e6c3cd1b1e08f47cd690bf6bd78aa1098765393f523b55f133faa2e74a39007b761fbcbb07cd568b56e13d97f48d6d05ac6ebd28c387faa09003b931ea11a4a81682a09b2faf9222d7eb5c46b7e268674df2500212ae2fd9882ca73b95ad151cb9edaeeb022bcc9ed786c242f2174c023f027338e70d8da48af8b5ec3118ad7c6044360d1c9e25999717ed8862278f6719c74d64703888cf41248978217bb897f29ccba02259238a185a76990b8e2bce0af5135e7fa53bd18efe3753d5834bb241f81e5b028989a1ca9dfd2cdb1c0161a6c3c360008cc5ec806e04048a222f047508e460000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000039bbebaa990e926f859e5bfed1357558c2124a0028942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae607bfc1dbb7cb8f1b249a8e884eda1ebc000a28cf62e95b0df40c7269a2e31c791ef0a7da327c4d71cb5450ea1f3543b6e8da8506e05284440f9959b52a792efb1796d4d1d6909d1c709e4ab5fc06e32f97b0ba3f034ffd11826e009679df5f1e037a93feb52babef495dbc27c7d5b7318075a53b00172545d1e1c27b36ea62a7021b66a1992b26c5c499c31513bc4867f52133f5af31194ccfd4523b2970a05d1697197d982c7d4e95c0c0f6e5616d70c351583909f9b77f10a1bbded80b1aeb1b1adace4e62472954e171bfa434d0adfcadfaf55c3fc416e9063465f2574cf31ad044497a6e3daf933033c136a027751ecf92974038fe19fa95cecc467313ea239ba876a0622d1bb13e885cd9ff2e88bc7c77681801abc32c075b87fd981fea000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000081e08338d5231e5b879e2b3fa560efd11026845228942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6284c21d93a655024f8afd34edc66f6d8d470a1f98a00b4922eb32d1e1b72aa7e0b27c333641719a86364a96c4d9ea44cb2fb35880828694eff5fad4efa64bbd62fdaa0d5996e556f76c0c444c305d3d632e920980968be6d90d819f49534df6e1213ce620eefe0592a99fb3f177ecc4e44fedb71d8273606e6910a765cd68ddb1d7d7942531eee25421628d3df18b6a99d6642a67adc17a837ea6f0377a817a70da666466dda5715aa8ebb905cff09ac31de68829f753dfb6ceaf07c604bc41e076e77c244e2683378c7999b298143e70e02fcce5f2702fa0c3f4ab0c9124ee905f3b92e7b9faf2439eb3b13dcc61f42f94573b26908753cd5e70dbfdce133bb12994979a1f5421f22debf7eb86384b3ea67eb028e5e914f0039f00a8dc8e49e000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000d22666a4ea994185639b9d49e08ff9c181973eba28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61c0055bc27c4c3824df4fee8de1a033fbc485da86d7dc60a9ff3e28f1af3d27b1e0c1e33bdce9a4506332df34b4a9ccf64a8d77b6a044541a73806ee2d5faad52331294b3992aebe6c164334ed7492afaa15629bcadb9ed6cf8174d12cce52560f5969705fafedc2e3cbd28a597a4e38f3ec9f6b53e98d841ecff6c3cab304c5106d67e8e4bb5256118639b83f9aee7efc605dfaa27f72f73310cdd8fcd7c6e304d834e86b013b3459218b0d8d5cfc9be53c4abe73f90faec9d3495f48a4a54c166a4cb2da9c3c82b5bb54504fb6742522c05038bd766037772b4ba54f14bdaa06dc0477627fe55db05d2be748ecec74752c54e0846a0733e54fc9ed8befdac12725f70d26f43b2059a186b4a801175432478ab9824c37d9c8ac62b28a6b0df9000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000006c7a88a3fffc0e32a65efe9dbca7ebef781ff3ac28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61f8c0ae0efac374c124895a71f82dbe39b17666162ed12523625cc781fce898b050aca11bb96a1c15a8c2c79fdb3d457ee992a0b8f9876dea6a747c052ee6e92104be0981bb2e19f7b49cff94ee4e96276150f5781f23857d6a3c2a328890a7c1022fec3967c9b22a33e6138319057985a9bc1bcb7dc7b52887071343222f0a3205239a8578dca33f4f0ff44605b669eae0f496e7f9532672504ec5b2764e89a163c6fa3d1a339df863a80e4b824e616491d736685b4b68fe008037ae9a0adb914ce67e76aca57400179a7f1a431d55477aa80ca9ce9dc2f1d68a1d5a938a85a2d22933370ea288614c7d3c13960a32240fd76652b8f6bcfb0190941ae8fae86034cacd834618c533e5546a772f9c9f660520c201788b2dd7e3c071c4bb41951000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000007e6a5fae2523fa310c0713f7c8cd34f1ee38470d28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae616d4c04c49348fc7844cc251d87fedfd8e810b1e0c2bd61c602a725a861f6910180ccedf030d7678cd567472eabe05e8cbb08ed34163e769644e69e757b2c7f92f2f0109fee8cae60cb587bf3b488903a61e1ca5225359c441fa749166129d6b2afa338de2ef2147f0ca5466679f03bfdef8da5db37b25d29a7ec2fc2648c0611675425fb9a9391caa9b8d7d3f540f02432ef65fbddb3e192a7dec2277bae81225e234f7b0e3364e71db446fec381e09df79ab6dffbfeb8259de6af1ebef1c740a68c98ed119b9e51aa27040fca58f7a00f30972bd21568879ddf0c246f560b822ff7c1650628b584e3a96c61be54b7ecd51bd779b2d4fd6a12a86999ab23515175649c71641fa898fed23379878fbfdccc932e0f4a478bf88af3e79c7653a90000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000dd75e013b66cbb2aa418cb4b2692dcfc9849f40e28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60d5da2b68f5d410c28aa577cceaf7b894664b06a96a97bc1d2753b53d23377531630c200b3d7643d136e6e9256aaea23676c20b94ef1c7f4888553674ea895941ea660085887d5b65bca0da94260fc4929d259e8f496e938fb276f0bfd055e3a19890b120f10c79a0615e1fb42da8e6deecd079eb31f20bbc50fde260e57cc4918fff935aec59948cd60f431d7aef81d2c5b5a59b046f52f6e2fe6748222c86c2e1135cf95e23fa167432e8b915db34e2efab40d3427dab386b85315981b28bd207751003741525798cc7234a8497890e6b163ee0ac99249eb8b5d43abd2087c2387c914d92d42aca318b7006115d4cfd6529b524ced840a95afc22a953659cf06d7ed44ae5a016eaa41b12a631ba51e8df6c0f4c7e1ce9e5c6902dd4ea6a785000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000000f05d6887b0233c339feb5ad2ff23f87bf41be6828942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61a8fe7eecded8555ec402387be0b88a0c5adc189fd8b71bbf871378a6941218f29da8ad7d1016ee49b9c6288f17277c24b61cdc0355438c73a641141d022605b025a62c611cf353712911d3f61b1fcec2c91ee2ab6192be544e49efb2a4a50510410b0e377e6f059612023201056e36d06b44096ff3c3e147c3c3ab28bf2c3e5281face614b7007cf82c485a522642582f67c08397a37350c3b38b5582af478f0013f1dee2866bd8e1d876135589e29f72aca4080e4a26e6bddacfb3e2fa50fa2ab1d6e70622cb9fae9e6fafcf80f5e0d93635cdf5f7483d6b9df0c34969d00911c0953b7fa85184ad0c2f24121cd8728dd6acf4ea0349ee6364e2e678ebb42627a13d8a8082907ea78ad804f951ca004c8b2573602283ad9fc6f15ff6b9d4f1000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000011a7587346dfa931b806ff5f9977109db91efddf28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae625be9d8bcb36aeff220e762650578f29ecd4e99d5210cbfd2498880c6bfdb5e92012a3eb720fd028528bcc103345ae1c4e877db28fff10c435c9cbbe421a0c0f282d3743a3cb2389fe9ef23dd8af75ab5b706c3cf506b77ff6c186ca9d3a63c70a94cce50ec196601cac07e33a5a566a2622eeccb46243930eaf6e0b1e482a651c51057ce621b43e9e2948a352968c612a62765f97b5c4027e81accecc0d028a0771d0572bd4d505c8560513cbccd42f4a255e9f0fdb5023d61014bd1130ed72007102fcd9a1a4eb0f4ea8a1ab4384b02125a84f7a2b1e0115c1d8a8db7ea8061c664ee452f5d5149348f4c2ebc23eccb626ab3b275ea17ae227f440eb7d00f91316b1fae327bf9dd48d422141c56322cf4b0253d249469658e0bae0a31583ed000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000007527959b2127f440ad9ab50968fa31acb221f8b428942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae621671e114dfdc998962474a624de9dc385015c89fba8c28bb62396a86fb3df0e17bf2ae07a798b9db4c5d1e73b49ebad41764cc4625f6bd67c30ee38e37a12fe1f84d347a39c94b27189de9a159247542fccd336eee271e696152b7e4197ab3218b64f72ed18a5685d5076610645fda0974379b025fc5b12f5e9ed852ae34bfc01361aa569e6aad7924edb221adb5b3983bb8d9b82bb3b4b742efda83a25c4512c67b28eeefc8dcab34d802c005d1baf023e2068e0500ee1288e41d95ced1b1518bb868523fe25efbb9ebc5928e762e993ba580ddc71f2b6645ce93b0e1ba66b02b33fb53b179483ce8a57f3c0d5903619b76834899295afb4a2e6d9a241d43a14211e2ba038fc5a42893eae0c785a2a02902ff8ac4206e6d8095540fa668191000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000f693bd3ca91bf811367096068c7f23175218a01e28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62cc2e17e4379c7c2d340a496320adff1a90cb195a9eb89f749cbe61cc301ed0716a7ad4fbf31b32e4d4db65789252bd299b23326b222bf197c94cffbfc97f75b26a7334cb7ee68ab9d8598f72eddb6922e685d77893724e8f13602f7ef539a752eefcbfd427dd9e64e0b53cba20f3c1686956359322cbe24f9343190dccfd7c628c4a19347913eb202533e53d7b3a86b9bcb188f922c8ac4e355d1352e67f7571f9c7c65bb2fbce018f4b9b6eb90f2453d2147d9ce48c153b665b11d2d4a891b0758c29655e377dde22872379459363e6d803dc1448d65506a97bc1c5b221b5a098aa4f9d28cd371668a1dae0bd62ff0700916596830a0580b203fcbffa5d3a119a87b6069429455c0b98105c7dc7666e5851a4d2a5179ab766e6581b912453b000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000e0e0504d9fcf343852a560834c1ccaf8b51f682728942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6072da586fed751ae8d3effd56ea2a96172b970f2b318c8471b3b2a8a56af968d10219530b160e8541a91a591ccd6ff5b396eafe24c322b9f991ed186c86288e120e49597383d85897069ffb2faa9ab3aea39c8e2bee30632a9d95fe4c632da02111f0096faaaf3572a5ed85b7c400e6e6c3f5f9a7906ec050f1e81184c9105aa292b1da0bd15c6de94f8ea3fa62b07d12f68c516ab0f68457acb6c583f3393651a4cf81688403773aef482427f22089cdf1ccb5df70a588ade26cc603578b50e1c2e473a5c11edd234697a81335a1a3468984728b8a1893a0d5f1f7226fd95a60fbfa60cdaa44d2ba9e1c50eac4026e1695e7c1c8cd9b05a6e67b2823355d3a72478b24e2564d9ada30f91edb4349ca84029a79237442bacb7508eee50ab3c1a000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000bf604969202162d64c0b3fd72c0e550aff8c9f9028942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61762942ae25b62df89794a14f0547240b5176a501a72f41434c51bd321b2ddd60601d70f80affb5ffd214152f77c69f6948ad44e0032056dcba35063bc336112116efa5196a6ca895df75fabed5706ec3e0822668e3f09c39c5415ee158a3f312c424a82ab819300183f392e946ccf5874cbde3d37bc6a07123e3a0dcba3a4ea14676b893285b2dc382ffbf64c876e9326f936791b36fe03c5e24a688e24379922458260dc9a934304b92642af551f7143973a291b7319c1d3137d253dfcf7df0d63b707edfdad155dae7387bdcbc5b48776c61db44ed51ecb723d25927a37cc2309a9f7a46582666701c40e2dd8c53dd058f0412376c3d4c9bcfb3acf83dc6a05275166c41c2c99a994b2b27686a27f86aac5a08b3a802a4717f6f7ac25709700000000000000000000000000000000000000000000000000000000c001a0d7070e7cd041624764220631f8a65be0e918d2c90107af534b57091c824b9389a02480f4c936f0ef4e9507b208e82baba87021525ee98b5941d08cfc0d7d146547").into(),
            hex!("02f920340a8306969b84032a47f984039387008349857a94087000a300de7200382b55d40045000000e5d60e80b91fc482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000001c000000000000000000000000000000000000000000000000000000000000003e0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000008200000000000000000000000000000000000000000000000000000000000000a400000000000000000000000000000000000000000000000000000000000000c600000000000000000000000000000000000000000000000000000000000000e8000000000000000000000000000000000000000000000000000000000000010a000000000000000000000000000000000000000000000000000000000000012c000000000000000000000000000000000000000000000000000000000000014e0000000000000000000000000000000000000000000000000000000000000170000000000000000000000000000000000000000000000000000000000000019200000000000000000000000000000000000000000000000000000000000001b400000000000000000000000000000000000000000000000000000000000001d600000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000cbf61a54ebe5e9152e1f6b81cbffd3027062642928942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61d460bd138ee32c939dfd8e49c57a95ef4dcef2f88b92e5a1d5e2905f86d787328bb4c7a1e4eb3b8db4d59a38321cd444d510a700551beb6fdfcdc88068c02f70506c41903052ed62b6913f66c9595a218d5726ea3128be80d62f7c69887816a22f613c865b39a6e228b0d90313dad896d11000afe422c2412aa8eef93fb1a1e0cd8be0d3226796e106d56c9f3d6ea5f189f88353aecab6565a256034ee48fd00bafdf7b018aa742f06a35f8ac0442c02d1a4f75186798a33cb57b53f3b2a09507ca58f2e94f1bc503faaabb19812e52adc5446d0d3a2eaa3c37c96cd6b3937014ef84689357e91ccced5dbfda2cbcff66f237923aa145326a831953620b7fc711e8faedc7ef58112a00cf6e13cbc0f005606720fb034d7a57f591ebcf33a66c000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000007cdb62cd44f27416d3d80060887b79edbc66eed428942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae627cd53af047b648a62ebbc6b7022738934e337451e6934e285bcb17ed151e9a2156cee556c24689d42cf8d19d308cdaf32215270100cb7de2300b61d6f3805e61fad747173364b42a1fbc54501d3fe6689204bb44e095ca5aca1652d4ce018070bbbad2a1406c2ed33020899c296fb67957c0dafa5e7630c6f15eed2d85ca3921f78322e092ee4289f1f00dbc46793221e1b2439129fb05b1f185cfa857baf812e9ba7fecd26f0671e1fbc3796ab226a8484095d31360f45222167cd7dab33f11a13d59918ab5dbcbd350d47b8d48d3b52e275887e0663b63b9cba34e97377af15def44fb6d6227bc92d17b372a4779d53d4117357d4e372f15ead01502b0cc3006542ebe7436861a0a015643024b8eb364ecc758dc76e38f5cbbf461c18a73b000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000060de5ca48792914e8ed36f8294c571cb64cbc86228942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61c2f5918a5841520443a45ccf49a742a50bcd4548fd8ad248a1b8742aa75b46417c1fc6a32babcf3b5960b1cfb35d98a261641322b811053ace79049d236019205960ca817a496c09847350c58899c0008ab73f124204dacff678139ae80c95c1d7877bf7f5c4b37f570321a76e604b5a140229e1564216e722102a0131c6f442933d8de999cee105ba504babd60abf0d97ff47ccf6395159a8c2a24d24ed918029a1cde683c17c594cad199facdf9928066a73025189be71671e2df5314c0cd00c501e761df7d4b8a0cc152d6870bdf5a3f5f7ad8bdbc2d0f11ce6cdcc829bc1acb667ba2d8c86243659458f23b0c5c46fef1761cc7013ce20c5a55bf9f28481c42002db11bd87e9f501a27ffa16297536ff3bd04d44cf8385c66913b259f94000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000004427395e8d53ec91fe009b9f9f7d108a6d046fa328942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60e8eb84cafb9b1049f380e248ee56e410da056b3ac4fae01e640bf76dfa9b32c089f372414a1aa9c722f3af366ab7f4dbf700d8cfd8376f90c5125df5211c0240c1b68b374253a99342db518fff6ecc0605c35d0abc5cb9fd58d13b554ca1204306006c533a0d4e84cbdc3d1a219f562df9e5afc90dd541b8b2332ba63eba2062b9aeaa5722b7ae0e7d60a977dfc2cc05bf7064b1ed2aa897c306236b1adea8528a850bbec99f23a3c0b9a747c2f285474f266424bf6d336d0c4ec119877f5a9109eafdf5d0f879f6907ecdc9cbd20f41d43dff8d94caf9c4486f017f7d10c511da1f9629b4d2e0232c0bf208a0f9408134b1f6fcf77b33ca549ea32536e43b900f4fbb5c755d3221871fdd7c7118c1fec5559871922b1b462a5f0502b9e14e9000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000ff7fa1fcf8df81ea158cfebdad80d00d49790c1b28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6142c5c102e9eb810eec6a0fe8cb214c224de98a8ae9e73f46897df36bffe668f15c5b544e21b5c7495900a1bcba9a3b9ff362e192a88c24be1834cecbbebcb172be37dcbd251b8ad00f6a589bf6a296eadba57dbebca0ec879f7c0b101c5a2d12849f2cb39082e9a68623d3793562d40de78acc164ece4ba819b9cd41c60ddb0155ce47cccdd81c62bd4850644937385e494c711b794ea61c3f876cfae88e05805ef8c12ea3890fb78f382fd4757b43fb31e9b711d5eb6179b302bc187b621e806dcecbe346e981b671c871b81d963a58c674ba7b5aaf83203e8b6480104b02006491322421291c323149a72e34c1c9c793b6797eb2a83408faba857a60d810f0f64bf23f7d507c895edfb71d617ccdc2f1f819e16bd07cb467ee77794bfc9b6000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000f34c1f7627c21d08affe689360b68e3b86bddd8e28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae603f4f9d9e44ecdfa78b3f822a00aac0a80fbde1675bed218b4e9daf3018c75c2274fc133f9df6704f4926e8201f000cdb3f5288f5bab86b24f4501734b001faa06b1f228701b430c5f77357668d11abb9aa246ec200026b8835fc8ea038dbc3721978e874ed6e196d0ee6ef1baed881ec1a8fdc818fca586e49adf1a01014d630b247457164a28f47dbef855ff71f85bc3d2d114f450bc209463a532cff56aa9183a29e8599a51c0a5a46435d21e0421a5ee0b31590e2670431f59948523a3840a6914dfcdaa1d8f4acc0999bff181168bc97f59d2e76914b8fe40da7cfd767510fcf9dce7c0aa00f582fcb232b250941dc3acdfb71328c97fb21804ad1abb181f04e30fe89e260a50e3ecff6b402e0796a12625cbb28e218565ca449bfc5d54000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000b6816921690bbb13022ce4279c741e9f10f6d25028942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60c7d7a2327da7193adb6e7da1a0011071990b12213c823686963a0fba1a43683034744575541f9edc32e3ce969ca106604b4a989340584ddc55b9728c7e2e735074515993dfc63a646b2d8f748414d5dbcd64394118e0f89d165c746caf2434209bb608dd08ab880aad6440c7340c1c684403cb97e2276590bd18963940d98eb1e1a272569a949e07dfe4a665893154eceb132e728555b80e40a51b6134c6a1e0a6fba21519df338be4c257b4a5300e8b8f22fd006f2e9fa8e2d72b60f3818e72de6b2584eda4a90fc2d78b98dc1b5ccbdf1f3c20e733bbff817158df22fbdba1d8e56cd2b62522af6b23891f4f2494e565ded4983941326df46f8895df90b261eddd0dbf8d65704970292fd40ea3c440b4f7004f8a988eaa0124ca04acc2f80000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000046cd855658bcbb261645dac5de9bcf3e86338ca528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61a678665a7ae5d9945c98f2448bf68e73e80475cbba2f1601c5be074171c21082ad67f4b03530e704fb1b232b3cddf83ed3cc000fcc0cc5137b6f4c3e89d59fe0f02287c98f5c39229b9ea0ecb3b2e927f66c5bd814bcf032c28d3b9227d3cb91154886e38171ac9cb09ad8f8a7898fe624c216c1f2753fdf2126f1e06adbecc0647e62c17a2c83ba39b269533b35a377b069f0b4ef684142f7f38360feb6e832ac89f308bf01ac0fe322a4d106b404f192b2c279299e97be3a5344516ca21ea105a99ffa98dca692c8c3f52addc90eda18cb19336aaa6d085e40463d74d3b8e0f317e0047e3c6f7f9ea5e99a7adf0afc1acffad89ac728808bb2ac3cd57c5aa2b0ccbc0976152d5b67b89e939da3ba4e0490d962b8035782ff36db7f05a23a3000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000004ef8a049f19848666c00d958723dda15310a88aa28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae629342c1f6c1e951de321ada9f9dd51886c7da9f435a971f21dcce3e8067d80b20edc4b5a3a2982f4bb2825f7e82dba0143325323491b7810c291cbcbd76e35242f9e523c196e71f608c01dc6fdda1f5a6fa0f60ee6422f0363313332b7f6c4ed2d2daaa8ded8a06a27765e33668632033a783e6a452d32f2d327f629e25106560c6251b41c8d03f2fd7617f7f3c745c899f32353223f8ab32b56f813e430391d0686a6604ba4f73d140c25c0d30a8f7aacc24696207d256f14a2d5f3e217f8a70a9254141d4351a6cfda264150d89b2a35140530990449b767f05689709488f00e227ac767806f1c3dfc1173d3886189057e5a55d147166005840d7ca50f17861700af52ea29cab029a6808d6dc6f82acaf942f9254adea7ac455916f0da4452000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000944618cd80dae4191c7f7f21c58ab13bdf81738a28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61b7951b069b65c12393920364d045f8d76add6c1d9f29245188781b12600d6be0d5d127d6bd399a6ef83d157093e1f5a7f3e9a1edf67993b678a7f6b2be9b65129827246aee4b32f7d33af5d8363c7edd2b8fa882103655c0e57999d5127aee113a7fe6d4e694ec0e96cca8218f2cac3ea45a958cdb8e7a77e5b0a7f65834f12178510f9a90038299b02ca079f79890aa550ecd77b0b88e6ada0b411255a560c0e214079f6191a523d2f8dac0a1426ddb1b7aaa51f4ab184341392492f727cad29d9439d19f56f626ff8a8482bd148134d5cad497b01d310680537b8e3dbad9705cc9230d48983aee1839334e024c271ea9560ced93bfa54b880901249fc647713b1931335356f6f2c35ac21de1c2a6c3302763a0eea4461c757fb18bf8bdad3000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000f2a05e7c62af3f2f02a24b43c2ae13b08890fd4628942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae622cde3d420dbb6ff32a2b0cae2a9e25945c539ad843317103de9e09595f424b328dada7a73b9e990c662830d82148ef6c90879c37ded18d7f24f9dea6c6cf01504184ca216206879d98b04f4e0eb97f6a6128a309e699e7d160201ed34f76d070c5377a5dc4baa602431b5034dce504c8e8fcb3033e605a7f9d43f5e9e4738c30c9021ba9dde79e0338b9843fbdfa7507174fbf43a28fa11b90f39bd4d0c697c0bc0c8292d6d79489b492324735a57da87a52f3fc56c83706711c86188e4734317b57e8482be2fbddd7b8999c6c403b1649ca77cae53ccb33a7466c0ba2e43670717bb2502de19dd83165e63ac0bdf45f628e77f314131766965730a44a947ab2d37be9216c866637b04e5ee682cb7fc17f92ce5c77cf9113abb1030a1997ab2000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000cbd0a779c26eb779769cc9d3a42f61c97c99602928942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60396cdbbacab4ca173328793d82669df272d72cbd5f355277324d1cf707995f82b320ecd46b4bc60e11394a891aa160cc8e87ac44ae214a23cddf356c8ef90990d7c8f0c9da0e89ce98f30d4b468df256397afae21430e552c0ab9a4fac037710c7ac7c8858a6d26323a0ef3e40bd3c62fccf473eaa83aebcf6b545a7bc6fd9105811a4341bdab25b827b79253700ac63f5df83cf602db53efa3c93aef66c99d0a5b385aa594eaa55cc8eba9fc5bb179bc1745e4fa1b988e58bd438c1b6a23be0a19671d950886a2c5d8adeb93b36d55c2a6a3fd577fa1f1645edd71a36c37581a5ff14720ee29643e09096dd01785828380c42c9dbe38d2fb89ea391af5deff2ebbaf11c9e1370f4786ae7deb76760ea476ebbbb2bdfce20159706bc2e18fd5000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000377a5093c2531aeaf4f72b4d0b2c48d3d6c8684a28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62375d5b21debe1c780d2dcf962266b2df4937663d6b6e93bcf22f1d64af660b51760dd6af28c91232ef017518ba734b91d159c4146074d8f8bdf5d0ca47f220f2d03765f35c93d02340b24d19aec48cbdc3f1fc9788ac2aad187ef5b9bc6bca6038a078c76db57cc594712c5c73c31afd34a7a441743799e3543968c33f76634138cd4cb56addade4da332152eee582c4bb8e416e45d3fc7f3f8ed0b7073337e2bcaf837d7d01edb32e38abf2439fa2aa55b077c1c7b60640d4dab03bf59c7f305c8ca9cd7a20b79807a5180f921a14acd7f30aa74882bc531c0b42c6e08d66904d07970d54795b0c30c383228090d4ed088af846c318e4de7c25ae9dc01e5352ed242f8f39a28c8dd178f364beed17820829d319729f3b3fbc85908339f1220000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000083b664be1badd9f38133826ed0d1051c473e474e28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61b96cb848a7076d907b26de483e289c97c2d7a37b74db23961df4cf3b7a77618019266266d91c62ff228e6016a2f9cb3635b8d7431b80a2284ea13e8692a049f0e81c6a92b69cf67abe95986bf1fd34f17bd458cf13668f47e11edc991a4aac313e246126ed023db3b6d19112f7acf890b77efb0300868a91e09d52a966e6d0315c21f20fe7630e06a0e648cd21e430e9cc12c920bb2df8ff89966830faae7243018fe0cb624573d66479c635f8d7ce04fa45292d39aafd96caeea287e08ca6210ee7e000e89bfbc77617a1f1c2c92bc25f4ca5bc4cb9fa8cff4039e27345baa09b9db170f8e1e9655ced30f72be9edefa5257ce44ad8cd3855cc9fcbaaa443923c51df11ed2f8b830c7bf83c18a77600b170ed08e7a0545c690e1357bc9cf4200000000000000000000000000000000000000000000000000000000c080a0b2ba45b0c4cabc8981799c254b90d90c019b757833e05c2742c13f52e82c7bcfa0296885d0ea2c68aa6020ac3881d89a110471efeef6327af2eb295481571c4386").into(),
            hex!("02f920340a8306969c84016c2d0e84039387008349cef494087000a300de7200382b55d40045000000e5d60e80b91fc482ad56cb0000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000e00000000000000000000000000000000000000000000000000000000000001c000000000000000000000000000000000000000000000000000000000000003e0000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000008200000000000000000000000000000000000000000000000000000000000000a400000000000000000000000000000000000000000000000000000000000000c600000000000000000000000000000000000000000000000000000000000000e8000000000000000000000000000000000000000000000000000000000000010a000000000000000000000000000000000000000000000000000000000000012c000000000000000000000000000000000000000000000000000000000000014e0000000000000000000000000000000000000000000000000000000000000170000000000000000000000000000000000000000000000000000000000000019200000000000000000000000000000000000000000000000000000000000001b400000000000000000000000000000000000000000000000000000000000001d600000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000050a76afb1154d01d6eba284069708d356d9969928942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6204472fcf9520f9fb2076ff97de783931cff4485b9028f260839f5a3518c0b7c2dedb9ba79cbf3ce5e0eca92a12da266d66f21c58bc107fb961e7fdce9538d4c195256b732f3eae1f49cde40c25294963146d06212e318cfba43ec8ec274d1bf22c8f77a99a5797161aad10de1fb3f3754df5fc20ccede01674a7c7284f7bd690484bc65d8c32f6a062c6e886f8563ce28e02d2e58485273eca71a27aeb19dc70860db898acf6d32b0140c8b90355e8b4b4bcc0d01f2761d44a573421bf4af1c0e6bcc3f1b2cd42af6830438342270b83e244cb2d680021a46c6a0416683e1e10fcb9e36c27546a590b8bfdaab06385ae9c72b22160622aa8acaf404bfafa50c2fd454af5d6f6d7dc38a9fd1dc115b8f1a55b7f9a2fac0e080cc6e7acf472955000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000fd176b56e03a2a4d400c61cade47630196614e6a28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61f480e5d05a2821f7b6a0da7912130ca375498b472fe02cdd1635bf57e3fc79d0e99986102ae4e27760a6d33cab28b4b77c5fc177b0030d8021b67017f7b4c931a69139b25a75a2023160dd6d6f02a21e95d3005308ce63c45c423de987d8e4b1dbec338be731bba82354b115902c3f4d702178bedea57978eb5b174459fc1b1270ea499e3a908cbda58d2c091f5616d9c2b18687adda6c24017833ce90acec200e87b2cc129df7a4f39d8f6e2c3b2a8d3f3e0905c3d4bff8ede401310ae1d9f276d6239c4c32dc9e627a1dbe6a60ea37e4f7de95512de72696e37de529686d0212ee16598faeb5445f37c581fd9a60bd3d22b1fd965a9cea3dd677df95c9fb3151ba48ffa3b5e7454aa32c6850bbed3457f6cccaac2f5cc9e399a68196b29e4000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000897e11089a878542836ec8e77b825a51ef8829b528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae600164253f8e62ae2671abad4b06d54a890f9abc01afec02287a15cf1115068951c9bdbc96cb8adaafae3aa83c4110616113c066688bb00fecc39bdeb64a1390a2917725d914fcc89e3c18a87a8ed00d9540700ec7320ab345baafb920b0441ad07c4a7e6b2dc434e9934277572be6f581fff0eb7be0857f48ddfa68c2e66cfce21fe5a394f6fcc4844335bae1029ebb014c6c08c2403aa11fbbf18cf6bffc425060e00ed6a571e020461981b6429019c35ef9f1be8bcbde05745a1c537b9472d03cba89ce21b3cb28eb03f08d7c97f1d94e8c0b55d3fd2586b74da048726ddbc1c8eb5ff64d606c6bacc97d6428a1ff83517a46b8f3d5194714a98957d2f1cd905a46fdce4364f2512312c238f9dc59ee6faffe30ee2d7f5887ab0eecd596b40000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000006bc063c2b6c1c94490329f632725e22237456b3f28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62ad99a00f80269a3a77de4fc3bd9976e9ef2a94954860a99c785b2bf1b63accc26de5151dac27c1c3d92571dfba7a67ca017482bee0be2188fa571875c660e5d221a475b95129123030032cf5243f78a2e6cf478c6c3c549d46fe6811a59c3900578aa1407face18fc14174b56571888ef0c471fd66aa52a53206215429d0f8405511b5377d2e625d010030d49045e1828e2366e8d5df27068e8eb9b89891b170a61b2f39e61d48c97c6117b1f04e5ac104311785063e3acc3cbe7358b13082204a43390431779d8fad127b39c5853f9926e94e6985a0fba609e7453db8a9e7c26d930a97127bc0cb549e5723f4b92f8cc7e5f800368ef108411f31e8c2476b80eac922344044afde8e958a3af9422dda01d35584d76526fc1a2a13ae59dbd0f000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000033200fe170f61ed4ac8a09b43e3798e4ac8f195b28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae62d3092e1316ce6abb3acc796d98205e786e22c1345c132b0e61070095b83993b136fae2ec9bb03d161cef5cad8bff5335812f32cea2191dfd42df867f64981542b7fc99748f6833af03f2d7c93ad8d3cfe6b5103a510deebbb029e4005dec5a201326eee2a5598f72b7500f0aedebc83563faef710c75cf0a8f69ee75645ecf0053f00c5847f869a0b27d58d6ca7314f590766b4e4eb305c3c31e64b206df10d02ae3cf650459570d00f74f002fdfa92e68bf056c8b974c07b189a33442281c51748d8cfeb75803b470f289144f38d5648bc6d2def5caf65d0cab9583dc0220f17c983c93039ed5fff9d82b233a207a1069f00bfee072c1ce04211e6571df5c825dc3cacbeb084a77f015460798ebf9a2a5e546013bbd245b57ccb3a3a60579b000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000f1ba6cef8ccb2990ce7de325b7fa407906125bf328942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae620c0644be433481651df7035bc5cc1247d8eb024f08f06646a9f1a81e475902711574aa82220fd71bc6cd5f1e7a2f1c66f6cdec680e04c449fd5f65f0606f84207dac5ae5563747e3e74aca6b92fe8be94a591ba654763f2c376a984ab6bac5a07cf890ae88d46cebfabe94398e2d8aef0a277caa1de32e3a81377e564286b1e02161e7871b40c0a388af9e732df270970cd74b6bb71ec71479a1144789da3ab279ba62160fbc729e5ce188fe5925e3ec9d04988f62adfbb9c8cef7246cd0d5720705aafd2bd6f4735dc9b19b03d606a70aabb86c38e2c3fdf2f53ef92e7eb64207335417ad2b241b88d9ae80418ca07eb457f2c557a5bd4c1eb2ef3c3fcbe3120e195a0a41d0dc2aec8703992711bc3ca34432f04012e6b771841b9862a7447000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000785cb80c25d8d3618ca7962b2dd85e7513b0fda528942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae6291ca2fbeb9e08a732b6937ff647309d7152fb770052fe37d3c7ca22911a6bec11b0f463565e65e5c255bb13afa8fb025bf41cc4aee8f4b8d00d1826c6ab90f813e024024e4c916186ca0ec32ab1688a6a09a156462027b218f39a1f4dd4b964243e8898fc270f9621c0a45380bcb5bc9b14a236680180a042258fac8aae0c9e1dd0ec39787afd536b51527426964ccbc2ddc1ae16b0d6f11d9241d1cb53b94e0e989e1624568bb59dea3cce55fd0b21696e6153034d29e5144b91afaebbc1ca28e9eacaa694821f267617c5cc3b5812f914315097b210e50c31cccc33dcc3310166b0c3a00a800e11be916b0e02875105e99127e4b458784594de119c5be391096f4f245bd3e19ce16a035f8f784a255e4b91bf5af58e6c8b25b0ea151a74c9000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000033090fc05f8e16cae75f332ebd31bb92135ee43028942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61293b778ea3a0add86d973685193aac98de614c89ed36878548b7e2a4abb37bf244289929b41c2ca77197403e44de06d465b59e36f189cbff475f4507f7676d0080338e4831274e7ebb54e58c60ad99e45273c61a7e8928f22a52784987e49d61d915117aeeeeeb6619849969af3e29831e98e5fef5b7ee074710aa86a76bd1c27b1e5fb6cd2e22786fce1f03564cdc8c1ed37a9a7d8f036351132437c16c2bf1edf147d96e5946d37803b5b60606971186736514563b075e15c871aa8072f552659fc082dc7dde5907057306ea98d5c1d82a96fc92a5ccd31fed314579e5ce92e1c0ef05de09ea78c2b62c8b4b51b33235fbea7ec6cd6fb6678ba6a81ecfb0b09b380a270901fe8ecf3b5c8875cfafcefc3ac58e44d7233756d97e0844234f5000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000006a63428000c4d1f428c3a7546dbc2b897336304a28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae612cd2880f852b8d726ec68101fa2ee3011c46c3e3c111a39db1356bba256244c2b88c6ff827347a3a00d18c54feff6f30519ac8f95a4851f05c148b15072f8b20aa1e91b77afa9bbd05aef46c2b90b138a1e6117c9d87a73f682e5db6ae890232a5a58a0063bd20022baba8317a33105e3f9ed5ff9786fe114c8bcf9c93d8d092bcda239c922faf406ae57773acab2fadebcf4fde5763153fabdbf198c36146010a39e592415cd62fc4eefe6e6f00c24aee3c626c1d8e4efe0ff664324d172441e993b5f28bb82d89c318f23c0c8b34b8d3d22fa692be9918ecc1e813144be9b014021eb71ccc2649f3a3c0cb7b56eccc633b41daee6f51fc3acca801201a69620784e86e0894942c024878b4b0040040851c2bf0ca42b34381e95fc28e28a28000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000e38fb18ff3738c39b1d9f9352f2468c4426ce4ae28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61c4102b911ae374a542f5c3c2629332d107f87c9d7f084e58ad6686b8ca032d02e4945598231c20938d0af0ce8567fb69936139c2ba3d6e8e1d9c7d986367a9f29a32ba151998a6732af0747596e22828161d91a3a92ed6664a99b7a0d988c2302b6f9af088ccf5d117a451d93d5b97a4555853aae24178279175905606d3af91c3f6e575377a0504d03e34de54894bc2caec9be3dac382a8d39be54a37069442f58d5cedd52832dddece59733cd2b4a6d370681b5a48b52bc76667902f20c0d1011a2c4396ef44f6003ae3bfb73948a1ecfa6f1855984b0d211ac0f4870a7682cd9ba5ecf6858e550b4fdc10893a899d9bc5327885047c271380499c3c0a7f51be5923b17ea743f23538d8b463fd7d142301174b8b9253f11b7e74310b4c17d000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb00000000000000000000000000000000000000000000000000000000000000230000000000000000000000006d012fe8cdfd1b3db63e95c8d7a06dc9e0b66aef28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae61f92267a1575d60651f0ca0a74d2ab67989f3ccc976b18246b504fca3df8cef31db8c4f95b1afeb35f826a69dca4995c5d2efcf3e9e459bbf99cca255ca1e8ea08c230b042609463c274ed7e6439e545d730c770ff035643c98c03bc419ef0c82eb48d9897932b8f919dc0684164b261d640c0a25e7064de8413beeb515d95121b47662c8e2fc34190f77919fa05e41226a1fa236d464cdd0c46e5d0822fdb4226ab9297b1ac2763b87897e4001bb5fd117ba894621c4f19291b31e074f4614523259e4884d735dbd2f0be3a5e687ccd4824a0a501cc311578f49a13b82beb0e18f3a1aca9688d95856c04b6a6c453397b08301bbb222c83e0a5ef28a116dce41c8ee0133596865178948427ffbfacf1856dbf317bc0d10ecb2ca1aa33dee293000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb000000000000000000000000000000000000000000000000000000000000002300000000000000000000000049c0d6a9e6f273c4494f3568e55d0a313c2edcfc28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae612c4c6415c49c164f8c81c76939fde44b1e6619d1946eb673ac203903dc36a00001c8c09a245beacc76f69f41def1da2219a0cd1ea328134e3aba603e29770fc1ef007c81d6fec56f2f1633d4a5f0b185fad2f719a8b92c624c1441eb2a2f9633020b4b1a0c2a6b16dea1fe908f0d5ab7c2d6f9d7ab50abd9135c25936c317a200f31f339962c0bba6bf3878088ee151d5163df24097350d02cc00f3d5f3a2e316554592f12df06777fc2eeb343641083f6299f66bd6f6f4ae3c125da8cc15680d35cfdfc71d15f59e6e37bae016dfc96da646aea2fbdba7be6fd334c53490ee008c2f1d877ed983a8afe7f98529da6e8186cb1874695070aeb5dc126d4235ad1fd76af0b23372d37b7f5181db3da8d2a46848cf4a473d55ccd23def6db02d46000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000485d8a98bb5dc09f1735aecbc8144ab2db7b545428942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae60543198b027d09bba2e1a841d4d3abd89256be8b84db977f5f651fbadf879efa05b9205ea53264deb3985cd04a071d90646a7f7f598d2caf2d2d005ad498943e15e5e0bbe91d868a0ee665e0651f8ffb6c6d2ac509c43d5bb53d805bc6c15f6d06909f64060d8c1a9519cf2b2c06a4542594ef2c3f03628ab85816700d9e6d4115ae7319fb08e0d7cc7755a766fe2be37a6f41332c900b23330c78cfb42e965b0fb6968782b33ec1dc44e79704c6eab3264b4a1a95caa47f3c1eeba76ab83dea260c18ce79fef579dacdc214b521666a9cb9c5ecd0ef82199e2b045ab073bd0926ccbdd591bacf2f04ce8b13437ec5ee71061c37a3bbd68879e0917ddf157ff5082b9767119e087ee6979a6ba346d99a2f6755aab877b680bd1973b7e6c96563000000000000000000000000000000000000000000000000000000000000000000000000000000007b46ffbc976db2f94c3b3cdd9ebbe4ab50e3d77d000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000184a41e6ceb0000000000000000000000000000000000000000000000000000000000000023000000000000000000000000231a2f44fde5788ee99c2ea163be421714ee47fc28942a4cc38d6ce636ded96f848bf0f3f547664e91b63f3daa080fbebc7d6ae624bd26434331113463b0801bad03295369f09b001551e3039be9a5c86f7c463d0fdd76dae5e472d7738264ac9db2ef6179e1d90a2e2f7931de1b862d24e4cacc171d8f1057095e652a2276423f70ec952dc43abb32bc72ca8080ae10451098b40aca120d5b8822848412087d67b40498eeab7246254d95db1b49b2b77838bee02597c0796b870fe29d3e0fcb8cd2d3148cc5d29ac0e0fe3cc1b42310b6f06cc90f771c1c8425d1e6ee1929cbc01986def1ea012780d606b41cf378682c9a0a2a13e44d09eac7239b532e1c5d99fc76a5b0c0e184edc31aea2b1ded286a8b7b8b1b8c744c3b5b68a413b7710a4106993e0e3aa97c148bbc46e7bd475e4a4a0bf71a64ba3b34be6656b6760ccd4b3c080019735cbf852cd50a5608d56ead05ffe900000000000000000000000000000000000000000000000000000000c080a0f613146bcf55b47f690f88aa4d95f8e188b28065b0ddbd49f689c7776a4049b3a00b4d55033092f80ca9df1123c6437f52372e2548cb8fbeca54722d9fe760e3c2").into()
        ];
        let payload_attrs = L2PayloadAttributes {
            fee_recipient: address!("4200000000000000000000000000000000000011"),
            gas_limit: Some(30_000_000),
            timestamp: 1717730355,
            prev_randao: b256!("c7acc30c856d749a81902d811e879e8dae5de2e022091aaa7eb4b586dcd3d052"),
            withdrawals: Default::default(),
            parent_beacon_block_root: Some(b256!(
                "a4414c4984ce7285b82bd9b21c642af30f0f648fb6f4929b67753e7345a06bab"
            )),
            transactions: raw_txs,
            no_tx_pool: false,
        };
        let produced_header = l2_block_executor.execute_payload(payload_attrs).unwrap().clone();

        assert_eq!(produced_header, expected_header);
        assert_eq!(
            l2_block_executor.state.database.parent_block_header().seal(),
            expected_header.hash_slow()
        );
    }

    #[test]
    fn test_l2_block_executor_big_block_2() {
        // Static for the execution of block #121135704 on OP mainnet.
        // https://optimistic.etherscan.io/block/121135704

        // Make a mock rollup config, with Ecotone activated at timestamp = 0.
        let rollup_config = RollupConfig {
            l2_chain_id: 10,
            regolith_time: Some(0),
            canyon_time: Some(0),
            delta_time: Some(0),
            ecotone_time: Some(0),
            base_fee_params: OP_BASE_FEE_PARAMS,
            canyon_base_fee_params: Some(OP_CANYON_BASE_FEE_PARAMS),
            ..Default::default()
        };

        // Decode the headers.
        let raw_parent_header = hex!("f90245a0e5d5cf15815d34d1f52079c2eade50abb5a4fb076f63276f8040e81116d87e72a01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a05605664e786f79b2312e293136d1aa6d5624181a59ad30bd11c91047100d2365a0cab880357793e1e6207e913de30eeaed08f7678c6d7822e8d5d44eec9b0ccd3ba073d6669bdb07a8d3fd21c24289e7176bd2890404acfd1978004c0da9f7ba4a48b90100008000000100200000c01080000000000000080000000020000c08000000000000001000030000010000003000800040000900c000000000040100400024000001000000000080000100000800000004000000000005200008001800800000100001000002000002000010000000082000004000090000000000685000a802000a00000008000000000004000000400000010001002080002000000820000802021000000004280200084200000001000080000000200000200080000000000040020082000000000000080000080000000000000000000000040000000060200090020800000080101000400000021000008400030008c000008000000108408084073862578401c9c3808312003c8466649e6780a023b3d2cb1b7216ef94837fdf94767b6235ce735a19de4f3feee7c1d603f2d10b880000000000000000840393d0c8a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a08ab0d68c0fc4fe40d31baf01bcf73de45ddf15ab58e66738ca6c60648676f9af");
        let parent_header = Header::decode(&mut &raw_parent_header[..]).unwrap();
        let raw_expected_header = hex!("f90245a0b87093412624e12fb30c80675628005b0c4abaaf3feeccb5b900f0824267a3fca01dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347944200000000000000000000000000000000000011a0c8201bf473cbf8adfaf240910aea025ea33573af383777a31407a4a4cf3cbfc7a064a9e23c5c2b545af4351d800e6d78dc4f209dc6253caad14751fdb79e45d0e6a0d95d3aa24da126f4a7ef04585d2f3ab7623e152305c3ba3da5231ab3cd6d3bd1b90100020000000101008180000100100000000000010000000000000c0204042100020291100002000000000002100000080000880000080220400400004000240b400000a00000004000a0801e091020040008000800012400000040000880100040000040210204000000000200010008400040000000100004000009b0000200000000020040000000002000001040110000012481002000001000010000804000820000401480080020480002400008809210006105000000000020001000000824201a1300000080002400000000000000c0000000004008000000004008608000104008ca0000000001000000000000020004200000094400001000000604428084073862588401c9c380831cc6d98466649e6980a023b3d2cb1b7216ef94837fdf94767b6235ce735a19de4f3feee7c1d603f2d10b8800000000000000008403910441a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b4218080a08ab0d68c0fc4fe40d31baf01bcf73de45ddf15ab58e66738ca6c60648676f9af");
        let expected_header = Header::decode(&mut &raw_expected_header[..]).unwrap();

        // Initialize the block executor on block #121135703's post-state.
        let mut l2_block_executor = StatelessL2BlockExecutor::builder(&rollup_config)
            .with_parent_header(parent_header.seal_slow())
            .with_fetcher(TestdataTrieDBFetcher::new("block_121135704_exec"))
            .with_hinter(NoopTrieDBHinter)
            .with_precompile_overrides(NoPrecompileOverride)
            .build()
            .unwrap();

        let raw_txs = alloc::vec![
            hex!("7ef8f8a0bd8a03d2faac7261a1627e834405975aa1c55c968b072ffa6db6c100d891c9b794deaddeaddeaddeaddeaddeaddeaddeaddead00019442000000000000000000000000000000000000158080830f424080b8a4440a5e2000000558000c5fc500000000000000070000000066649ddb000000000131eb9a000000000000000000000000000000000000000000000000000000023c03238b0000000000000000000000000000000000000000000000000000000000000001427035b1edf748d109f4a751c5e2e33122340b0e22961600d8b76cfde3c7a6b50000000000000000000000006887246668a3b87f54deb3b94ba47a6f63f32985").into(),
            hex!("02f8740a83180225840bebc200841dcd6500830249f0944ccc17a0cb35536d479d77adea2f8aa673e9b2d38703e871b540c00080c001a09e25fc89301001dc632d96e4ae9141953689fe57bb5f4925bb52d8ad113cec049f2a5e231b2ecf6a7f541b466d8f87445cb467cd5f632d7ffc2999c7dc580a5b").into(),
            hex!("02f8750a83180226840bebc200841dcd6500830249f094212f4275087516892c3ee8a0b68fcf9c13f6edbd8703e871b540c00080c080a0df653a88e57f031682ef1e97ebed3cade860186ce6a2a1e56065929d1f9570aba075e2c34f0abad126a901d7947fdb8640d4b3d0c096b70be77cbd23f963645eb1").into(),
            hex!("02f8750a83180227840bebc200841dcd6500830249f0944b178ebb31988b0b7df425e2fa9247113c9a9a8e8703e871b540c00080c001a06cf51a0ce263413929c7219f2696da810b1ac0b24c19417ed124f48f426b28e5a06caab14e2e884b1e6e4940b88a834a92465351db58a3b4abdc638adb2877ed6a").into(),
            hex!("02f8750a83180228840bebc200841dcd6500830249f094169e79e9e33763aeba5c1eee6b265eb24a24b0598703e871b540c00080c001a0077aed7927363b13ee91ec719b9cbba48227c50ea80e72829e4c54a492402be7a034ffcf5d50d3821665266e217cb3edaf43aadd99837b14b36dd97054db837eeb").into(),
            hex!("02f8750a83180229840bebc200841dcd6500830249f0941677c2a6d05056651bd8eeffb8834037a86b5a598703e871b540c00080c001a03f4c22cec9402f9d04c1f1e92406cc5a53ccccc906430047382ee0c477172990a0141ed5488e7b89eb22484e2f3685ade5933f8e4d6ae03c55722f81c48ce3e6ec").into(),
            hex!("02f8750a8318022a840bebc200841dcd6500830249f094613849638013429abca858d60bab0c991da5e3dd8703e871b540c00080c001a0674d1cf0c2ec10ec8cf3ddf1ceb13d334b95e508b05ffb6ea07f3f1018c8886ca01b89992a74e451b929430e516529379abe853615b605669251a162123b6a319c").into(),
            hex!("02f8750a8318022b840bebc200841dcd6500830249f094958f564d5a9586deee8cde55676400810796779c8703e871b540c00080c001a0d0dc11affeaa5f379ab4a8995cd00d7324cb7a6875c48f3ac8343c11d47b7ee1a02453699b07195ea21a807f88f39c01a1d809d45654399a2e4c852ca177eaaaed").into(),
            hex!("02f904780a0c84068e778084068e77808302df9894ef4fb24ad0916217251f553c0596f8edc630eb668718cfb10af2a4f7b90404b930370100000000000000000000000000000000000000000000000000000000000000c00000000000000000000000000000000000000000000000000000018ff90a61f40000000000000000000000000000000000000000000000000000000000000340000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000003600000000000000000000000000000000000000000000000000000000000000380000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000154232662c24f700000000000000000000000000000000000000000000000000000000000001600000000000000000000000000000000000000000000000000014c61b1fedfc00000000000000000000000000000000000000000000000000000000000000a4b100000000000000000000000000000000000000000000000000000000000001a000000000000000000000000046eef3e36b4e393922fa3c32e9f43f899b3f712700000000000000000000000000000000000000000000000000000000000001e000000000000000000000000000000000000000000000000000000000000002200000000000000000000000000000000000000000000000000000000000000240000000000000000000000000000000000000000000000000000000000000026000000000000000000000000000000000000000000000000000000000000000140000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001446eef3e36b4e393922fa3c32e9f43f899b3f7127000000000000000000000000000000000000000000000000000000000000000000000000000000000000001446eef3e36b4e393922fa3c32e9f43f899b3f71270000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000410101010000e60075efbc77000000000000000000000000000000fced1f1bc61400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c080a0bd38744150a6226dd2a4d86fb406292059fb7e9599ad2ff0968815554aaa7127a01857d067cc9495cb8678d42732d1ee2717438ee248fc0c11c4881fd0e573905c").into(),
            hex!("02f8ae0a0c83cc52e084062efafe829db294420000000000000000000000000000000000004280b844a9059cbb000000000000000000000000efb3cc77ebb75333112cc61ef19772f9155d2add00000000000000000000000000000000000000000000005150ae84a8cdf00000c080a05ec4c586fbbdee52dfaa903b1151a48bc5df2cd210e2a0d87daf15b433203467a060e3ee29b6d862c6ec0d1f91b241b8e45a142fda4dc69458ff2cac8738dd37b9").into(),
            hex!("02f904130a8308267e839896808406538c1d8307a12094903f58ee6d6c3c2ca26427c8f917f6ae515827b180b903a4c9807539000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000002600000000000000000000000000000000000000000000000000000000000000300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001c000000000000000000000007c2b7b4ac0fcc188b3e7ff5b83f6597f0005ed340209010308020507000406000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000b575892db6100000000000000000000000000000000000000000000000000000b5c6a298fef00000000000000000000000000000000000000000000000000000b5c6a298fef00000000000000000000000000000000000000000000000000000b5c83646d3300000000000000000000000000000000000000000000000000000b5c8538f80000000000000000000000000000000000000000000000000000000b5c8538f80000000000000000000000000000000000000000000000000000000b5c8538f80000000000000000000000000000000000000000000000000000000b5c8baa58fb00000000000000000000000000000000000000000000000000000b5c99feb2d700000000000000000000000000000000000000000000000000000b5c9c9f4a780000000000000000000000000000000000000000000000000000000000000004dc74097b0e5cc36a7cdf0722fb9608cf7e9a5a535475e34b78efbc9a2ef2555cf24b376824df6cfc3fe2db6e24797c1efa2ff2dbb64d34fac96fa7b7fa31a8e1480b1b684b1bc449c33d8f1709efe37cce06183ceae81c69833c48ebcb1a216ce73e8eb98ce8cc7e71d39b5636b91951e671bce12498e604b57be8bff88e448f00000000000000000000000000000000000000000000000000000000000000044a328613d73f44e21da13f28f233958119f314e9e1ebe1f4541eb209436c36c16618e2edf999aa2d263d5a6734f6a341143633023235a81ccda6917bddfa340e5b2f76db20be0dbae19af99a2430e3f0c111880eaa98cd95580b441a1fc2621645e5d7610a6697a0178c67d6fa3812710c179e6a5c7ef0f607b19aedfeedc39cc001a0303b7d9a441823918d302b04cb58ca6b5c834d952af0429709a717ed2316a48da03d9a9af29b52b6d33678b79b6e794eea4e12519b7de717c9ccf495b2d4ee6f13").into(),
            hex!("02f8b20a832cfaab834c4b4085012a05f20082cc149494b008aa00579c1307b0ef2c499ad98a8ce58e5880b844a9059cbb0000000000000000000000008bfd47c9c6ff6ae8757f2bbe78e585471348fb72000000000000000000000000000000000000000000000000000000000213c4d0c080a0dead8785821ccedfe19e14a59822a6798766268dc5eca2179e0be9f05f8e2431a053ef2c483ab1eb1d2feea63a2e3f40769f8cbe419853a4213e7bdd147cc64842").into(),
            hex!("f869018403b83b4a82561394a061b7f22ba72df1bd64fc8980a66df01a9b21d086026f910a80f28038a0c43718694719ebcc8116e6ae21b1ab64487c43a56b4b3e78ae9a361a7ea57614a056f90d2b9f6e8a1776d61ed6eaf831e6e1c64310ebfd34e5b828bac83557a44d").into(),
            hex!("f869018403afd9a6825dd894a061b7f22ba72df1bd64fc8980a66df01a9b21d086021ce0d33aa58037a0b471d2ea71e7d125e64ffd3d141318ce949a40cd0b39cca01625c41e57dead89a073f5fc7c2b79e98053907cd62dac09ed86eae297acd7f22c44ea11ab8a1ca26e").into(),
            hex!("02f904da0a83012990830f424084045e2f258305e39c9400000000fc04c910a0b5fea33b03e0447ad0b0aa8702e5763b5ce075b90464a44c9ce700000000000000000000000000000000000000000000000000000000000000600000000000000000000000000000000000000000000000000000000000000160000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029d7230c7d0257d69880fa5255bcb14863b0e6d100000000000000000000000000000000fcb080a4d6c39a9354da9eb9bc104cd7000000000000000000000000000000000000000000000000000000006664a0bd00000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000041fe4bb48f83d1fba43c9f77e98386c43cf86872532b0b047c7c53080bf36e7d823a2206018d9ca46a93c21385de8ea5f2d37d5d9d3980cd4a89807ce1a2a415e51c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000006664a0bd00000000000000000000000000000000000000000000000000000000000002400000000000000000000000000000000000000000000000000000000000000020659a6d1a52778774ecc59ebdd05f205d8713b62ed4160cb76474c091f1cdb5f60000000000000000000000000000000000000000000000000000000000000120000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000023c000000000000000000000000002ef790dd7993a35fd847c053eddae940d0555960000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000006664a0bd000000000000000000000000000000000000000000000000000000000000004139457ec7958ea305974201cf59c768e680ad098fd5b174d4653eb99e2077a9bc270b641cd87b6f4e52eba7d072cf169eb5c1fe995e442f0f7fd82df12bf2416f1c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000415056e9bcf742ba1be88d2fe14568d67f70bdf3c230a425d2152fa8d4097d68ba125c520e1e3a778992f0978913ccf51364799ac9a24a30dc70d156f67eff9a891b00000000000000000000000000000000000000000000000000000000000000c001a04ff321d98b0de90b4149b0e99ba8d291c6208d63d50fad85574e51f6eebbadb4a061590ed8c64190dcb7934c10073c670876c5e62de3eb17d8ffd5b061cf12d922").into(),
            hex!("02f906770a81a0830f42408407566a76830927c09400000029e6005863bb2e1686a17c4ae0d17236698694b19b4eb054b9060424856bc300000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000208010000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000004000000000000000000000000000000000000000000000000000000000000000a00000000000000000000000000000000000000000000000000000000000000040000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000005af3107a4000000000000000000000000000000000000000000000000000000000000000049400000000000000000000000006d4b5289b981933e34af10817f352061bad6353000000000000000000000000000000000000000000000000000000000000012000000000000000000000000042000000000000000000000000000000000000060000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000005ae1b619cd6b0000000000000000000000000000000000000000000000000000000000000001800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000410000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000000014000000000000000000000000000000000000000000000000000000000000001c00000000000000000000000000000000000000000000000000000000000070e3c000000000000000000000000000000000000000000000000000000000004e56c0000000000000000000000004939d67eeb427312c86d9f889bd7d90cbd1ca2660000000000000000000000000000000000000000000000000000000000a909920000000000000000000000000000000000000000000000000000000008e068db00000000000000000000000000000000000000000000000000000000000151800000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004114000000000000000000000000000000000000000000000000000000000000000000000000000000000000000006d4b5289b981933e34af10817f352061bad635300000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000004114000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000029e6005863bb2e1686a17c4ae0d17236690000000000000000000000000000000000000000000000000000000000000000000029e6005863bb2e1686a17c4ae0d172366900000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000109000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000000400000000000000000000000004939d67eeb427312c86d9f889bd7d90cbd1ca2660000000000000000000000000000000000000000000000000000000000000000000000000000000000000000c001a0412f0c2e72d680c2fedd9c1282ca4935f0646baea439ea372fa28a3febfcf648a0572980c28d5ba9522f592b7f3d39a94b5f5b123a62f6bb136685b12a1d9e7c52").into(),
            hex!("02f904da0a83012933830f424084045e2f258305e39c9400000000fc04c910a0b5fea33b03e0447ad0b0aa8702e5763b5ce075b90464a44c9ce7000000000000000000000000000000000000000000000000000000000000006000000000000000000000000000000000000000000000000000000000000001600000000000000000000000000000000000000000000000000000000000000000000000000000000000000000ff5f082dbf2d2f930eb3d2b51bb2f1010a4d5a8900000000000000000000000000000000fcb080a4d6c39a9354da9eb9bc104cd7000000000000000000000000000000000000000000000000000000006664a0bd00000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000041819edf15e18a545cbbabbd24c9f3e136a3dcec10c65f5ce19d6fb903e586e6db5f2e154210d25768487e89cb7d9b62cc611421c3456538c7d9ac39f0fd619e111c0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000000000c000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000000000000006664a0bd000000000000000000000000000000000000000000000000000000000000024000000000000000000000000000000000000000000000000000000000000000205183a065a3c67e765796c2969e67b960d2ae041a36070a7ed719b18b5682bdda0000000000000000000000000000000000000000000000000000000000000120000000000000000000000000000000000000000000000000000000000000002000000000000000000000000000000000000000000000000000000000000023c000000000000000000000000002ef790dd7993a35fd847c053eddae940d0555960000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000006664a0bd0000000000000000000000000000000000000000000000000000000000000041dd6bb97a2c5250e07ba4b0df71715c81ac9adc5ee1111a0f8ff3845321ac7d5262a7cdd80df46391bc4e41b5f1492110a42badfb7b6ebdd8944e2c3c9a9d8e621c000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000041d5ae1701d2a1750e608e1f2b5d10280db952ddb14cc35f819f7334e9eb5133a5072108be5f5134ac6c061fa9e3bcee464ad8c8e13e206cd5c6b632a7294cd34b1c00000000000000000000000000000000000000000000000000000000000000c001a0994107041475defdb265cdbe6da3df610516ec50a81dd9c0a3602c20d1c72b22a04b28a32d391ebdc15e63ffd26dc3a91e6c3c0565538e33e04558a9d1c9d70b3f").into(),
            hex!("02f8b50a01830f424084045a3cc98306213f942a5c54c625220cb2166c94dd9329be1f8785977d866e5ceb32785cb844f5c358c60000000000000000000000000000000000000000000000000000000000000504000000000000000000000000000000000000000000000000000000000263b143c001a051a2584f761c7ef2216652d41840cb1e84c0576d076c72f06906777cb043d45aa02bdbb191600306e5f3ff6d65991f61979f8a10d60d0c6ccaaf1f6d5c7515dac2").into()
        ];
        let payload_attrs = L2PayloadAttributes {
            fee_recipient: address!("4200000000000000000000000000000000000011"),
            gas_limit: Some(30_000_000),
            timestamp: 1717870185,
            prev_randao: b256!("23b3d2cb1b7216ef94837fdf94767b6235ce735a19de4f3feee7c1d603f2d10b"),
            withdrawals: Default::default(),
            parent_beacon_block_root: Some(b256!(
                "8ab0d68c0fc4fe40d31baf01bcf73de45ddf15ab58e66738ca6c60648676f9af"
            )),
            transactions: raw_txs,
            no_tx_pool: false,
        };
        let produced_header = l2_block_executor.execute_payload(payload_attrs).unwrap().clone();

        assert_eq!(produced_header, expected_header);
        assert_eq!(
            l2_block_executor.state.database.parent_block_header().seal(),
            expected_header.hash_slow()
        );
    }

    // TODO: Add a test that uses a block where the output root was confirmed on chain
    // to test the `compute_output_root()` function.
    // Example (verify at index 8833 on 0xdfe97868233d1aa22e815a266982f2cf17685a27):
    // - Block: 121184863
    // - Output Root: 0x3ea8b0e09b39e9daa1b1520fe59faef02de3656d230d876544952cbc44d6d71f
}
