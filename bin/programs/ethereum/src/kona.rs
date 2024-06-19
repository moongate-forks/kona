use crate::{BlockWithSenders, Header, InputFetcher, B256, U256};
use alloc::sync::Arc;
use alloy_primitives::Bytes;
use alloy_rlp::Decodable;
use anyhow::{anyhow, Result};
pub use kona_client::l2::TrieDBHintWriter as TrieDBHinter;
use kona_client::{CachingOracle, HintType, HINT_WRITER};
use kona_mpt::TrieDBFetcher;
use kona_preimage::{HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient};
use reth_primitives::keccak256;
extern crate alloc;

pub struct InputFetcherImpl {
    oracle: Arc<CachingOracle>,
}

impl InputFetcher for InputFetcherImpl {
    fn new() -> Self {
        /// The size of the LRU cache in the oracle.
        const ORACLE_LRU_SIZE: usize = 1024;
        let oracle = Arc::new(CachingOracle::new(ORACLE_LRU_SIZE));
        InputFetcherImpl { oracle }
    }

    fn get_block_with_senders(&self, block_number: u64) -> Result<BlockWithSenders> {
        let block_number_be = block_number.to_be_bytes();
        let input_hash = keccak256(block_number_be.as_ref());
        kona_common::block_on(async move {
            // Send a hint for the block header.
            HINT_WRITER
                .write(
                    &HintType::L1BlockWithRecoveredSenders.encode_with(&[block_number_be.as_ref()]),
                )
                .await?;

            // Fetch the header RLP from the oracle.
            let serialized_block_with_senders =
                self.oracle.get(PreimageKey::new(*input_hash, PreimageKeyType::Keccak256)).await?;

            // Decode the block header
            todo!()
        })
    }

    /// This is used for fetching the parent header, in our context.
    fn header_by_hash(&self, hash: B256) -> Result<Header> {
        kona_common::block_on(async move {
            // Send a hint for the block header.
            HINT_WRITER.write(&HintType::L1BlockHeader.encode_with(&[hash.as_ref()])).await?;

            // Fetch the header RLP from the oracle.
            let header_rlp =
                self.oracle.get(PreimageKey::new(*hash, PreimageKeyType::Keccak256)).await?;

            // Decode the header RLP into a Header.
            Header::decode(&mut header_rlp.as_slice())
                .map_err(|e| anyhow!("Failed to decode header RLP: {e}"))
        })
    }
}

pub struct TrieDBFetcherImpl {
    oracle: Arc<CachingOracle>,
}

impl TrieDBFetcherImpl {
    pub fn new() -> Self {
        /// The size of the LRU cache in the oracle.
        const ORACLE_LRU_SIZE: usize = 1024;
        let oracle = Arc::new(CachingOracle::new(ORACLE_LRU_SIZE));
        TrieDBFetcherImpl { oracle }
    }
}

impl TrieDBFetcher for TrieDBFetcherImpl {
    fn trie_node_preimage(&self, key: B256) -> Result<Bytes> {
        // On L2, trie node preimages are stored as keccak preimage types in the oracle. We
        // assume that a hint for these preimages has already been sent, prior to
        // this call.
        kona_common::block_on(async move {
            self.oracle
                .get(PreimageKey::new(*key, PreimageKeyType::Keccak256))
                .await
                .map(Into::into)
        })
    }

    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes> {
        // Fetch the bytecode preimage from the caching oracle.
        kona_common::block_on(async move {
            HINT_WRITER.write(&HintType::L2Code.encode_with(&[hash.as_ref()])).await?;

            self.oracle
                .get(PreimageKey::new(*hash, PreimageKeyType::Keccak256))
                .await
                .map(Into::into)
        })
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header> {
        // Fetch the header from the caching oracle.
        kona_common::block_on(async move {
            HINT_WRITER.write(&HintType::L2BlockHeader.encode_with(&[hash.as_ref()])).await?;

            let header_bytes =
                self.oracle.get(PreimageKey::new(*hash, PreimageKeyType::Keccak256)).await?;
            Header::decode(&mut header_bytes.as_slice())
                .map_err(|e| anyhow!("Failed to RLP decode Header: {e}"))
        })
    }
}
