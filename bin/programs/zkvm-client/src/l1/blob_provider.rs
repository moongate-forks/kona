//! Contains the concrete implementation of the [BlobProvider] trait for the client program.

use crate::{BootInfo, InMemoryOracle, CachingOracle, HintType, HINT_WRITER};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use alloy_consensus::Blob;
use alloy_eips::eip4844::FIELD_ELEMENTS_PER_BLOB;
use alloy_primitives::{B256, keccak256};
use async_trait::async_trait;
use kona_derive::{
    traits::BlobProvider,
    types::{BlobProviderError, IndexedBlobHash},
};
use kona_preimage::{PreimageKey, PreimageKeyType, PreimageOracleClient};
use kona_primitives::BlockInfo;

use hex::FromHex;

/// An oracle-backed blob provider.
#[derive(Debug, Clone)]
pub struct OracleBlobProvider<O: PreimageOracleClient> {
    oracle: Arc<O>,
}

impl<O> OracleBlobProvider<O>
where
    O: PreimageOracleClient
{
    /// Constructs a new `OracleBlobProvider`.
    pub fn new(oracle: Arc<O>) -> Self {
        Self { oracle }
    }
}

#[async_trait]
impl BlobProvider for OracleBlobProvider<InMemoryOracle> {
    async fn get_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[IndexedBlobHash],
    ) -> Result<Vec<Blob>, BlobProviderError> {
        let mut blobs = Vec::with_capacity(blob_hashes.len());
        for hash in blob_hashes {
            blobs.push(self.get_blob(block_ref, hash).await?);
        }
        Ok(blobs)
    }

    /// Constructs a new `OracleBlobProvider`.


    /// Retrieves a blob from the oracle.
    ///
    /// ## Takes
    /// - `block_ref`: The block reference.
    /// - `blob_hash`: The blob hash.
    ///
    /// ## Returns
    /// - `Ok(blob)`: The blob.
    /// - `Err(e)`: The blob could not be retrieved.
    async fn get_blob(
        &self,
        block_ref: &BlockInfo,
        blob_hash: &IndexedBlobHash,
    ) -> Result<Blob, BlobProviderError> {
        let mut blob_req_meta = [0u8; 48];
        blob_req_meta[0..32].copy_from_slice(blob_hash.hash.as_ref());
        blob_req_meta[32..40].copy_from_slice((blob_hash.index as u64).to_be_bytes().as_ref());
        blob_req_meta[40..48].copy_from_slice(block_ref.timestamp.to_be_bytes().as_ref());

        // Fetch the blob commitment.
        let mut commitment = [0u8; 48];
        self.oracle
            .get_exact(PreimageKey::new(*blob_hash.hash, PreimageKeyType::Sha256), &mut commitment)
            .await?;

        // ZKVM Constraint: sha256(commitment) = blob_hash.hash
        assert_eq!(<[u8;32]>::from_hex(sha256::digest(&commitment)).unwrap(), blob_hash.hash, "get_blob - zkvm constraint failed");

        // Reconstruct the blob from the 4096 field elements.
        let mut blob = Blob::default();
        let mut field_element_key = [0u8; 80];
        field_element_key[..48].copy_from_slice(commitment.as_ref());
        for i in 0..FIELD_ELEMENTS_PER_BLOB {
            field_element_key[72..].copy_from_slice(i.to_be_bytes().as_ref());

            let mut field_element = [0u8; 32];
            self.oracle
                .get_exact(
                    PreimageKey::new(*keccak256(field_element_key), PreimageKeyType::Blob),
                    &mut field_element,
                )
                .await?;

            // TODO (zkvm constraint): opening(field_element_key[..48] at field_element_key[72..]) = field_element
            // This will require c-kzg or similar
            blob[(i as usize) << 5..(i as usize + 1) << 5].copy_from_slice(field_element.as_ref());
        }

        tracing::info!(target: "client_oracle", "Retrieved blob {blob_hash:?} from the oracle.");

        Ok(blob)
    }
}

#[async_trait]
impl BlobProvider for OracleBlobProvider<CachingOracle> {
    // TODO: Can I get rid of this duplication? It doesn't let me implement some methods
    // generically and some specifically unless I add a default impl to the trait itself.
    async fn get_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[IndexedBlobHash],
    ) -> Result<Vec<Blob>, BlobProviderError> {
        let mut blobs = Vec::with_capacity(blob_hashes.len());
        for hash in blob_hashes {
            blobs.push(self.get_blob(block_ref, hash).await?);
        }
        Ok(blobs)
    }

    /// Constructs a new `OracleBlobProvider`.


    /// Retrieves a blob from the oracle.
    ///
    /// ## Takes
    /// - `block_ref`: The block reference.
    /// - `blob_hash`: The blob hash.
    ///
    /// ## Returns
    /// - `Ok(blob)`: The blob.
    /// - `Err(e)`: The blob could not be retrieved.
    async fn get_blob(
        &self,
        block_ref: &BlockInfo,
        blob_hash: &IndexedBlobHash,
    ) -> Result<Blob, BlobProviderError> {
        let mut blob_req_meta = [0u8; 48];
        blob_req_meta[0..32].copy_from_slice(blob_hash.hash.as_ref());
        blob_req_meta[32..40].copy_from_slice((blob_hash.index as u64).to_be_bytes().as_ref());
        blob_req_meta[40..48].copy_from_slice(block_ref.timestamp.to_be_bytes().as_ref());

        // Send a hint for the blob commitment and field elements.
        HINT_WRITER.write(&HintType::L1Blob.encode_with(&[blob_req_meta.as_ref()])).await?;

        // Fetch the blob commitment.
        let mut commitment = [0u8; 48];
        self.oracle
            .get_exact(PreimageKey::new(*blob_hash.hash, PreimageKeyType::Sha256), &mut commitment)
            .await?;

        // Reconstruct the blob from the 4096 field elements.
        let mut blob = Blob::default();
        let mut field_element_key = [0u8; 80];
        field_element_key[..48].copy_from_slice(commitment.as_ref());
        for i in 0..FIELD_ELEMENTS_PER_BLOB {
            field_element_key[72..].copy_from_slice(i.to_be_bytes().as_ref());

            let mut field_element = [0u8; 32];
            self.oracle
                .get_exact(
                    PreimageKey::new(*keccak256(field_element_key), PreimageKeyType::Blob),
                    &mut field_element,
                )
                .await?;

            blob[(i as usize) << 5..(i as usize + 1) << 5].copy_from_slice(field_element.as_ref());
        }

        tracing::info!(target: "client_oracle", "Retrieved blob {blob_hash:?} from the oracle.");

        Ok(blob)
    }
}
