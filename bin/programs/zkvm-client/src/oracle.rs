//! Contains the host <-> client communication utilities.

use super::Precompile;

use kona_common::FileDescriptor;
use kona_preimage::{HintWriter, OracleReader, PipeHandle};
use cfg_if::cfg_if;

use alloc::{boxed::Box, vec::Vec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use hashbrown::HashMap;
use kona_preimage::{HintWriterClient, PreimageKey, PreimageKeyType, PreimageOracleClient};
use alloy_primitives::{keccak256, Address, address};
use sha2::{Digest, Sha256};
use serde::{Deserialize, Serialize};

/// The global preimage oracle reader pipe.
static ORACLE_READER_PIPE: PipeHandle =
    PipeHandle::new(FileDescriptor::PreimageRead, FileDescriptor::PreimageWrite);

/// The global hint writer pipe.
static HINT_WRITER_PIPE: PipeHandle =
    PipeHandle::new(FileDescriptor::HintRead, FileDescriptor::HintWrite);

/// The global preimage oracle reader.
pub static ORACLE_READER: OracleReader = OracleReader::new(ORACLE_READER_PIPE);

cfg_if! {
    if #[cfg(target_os = "zkvm")] {
        /// The global hint writer when in zkVM mode (no op).
        pub static HINT_WRITER: NoopHintWriter = NoopHintWriter {};
    } else {
        /// The global hint writer.
        pub static HINT_WRITER: HintWriter = HintWriter::new(HINT_WRITER_PIPE);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InMemoryOracle {
    cache: HashMap<PreimageKey, Vec<u8>>,
}

impl InMemoryOracle {
    pub fn from_raw_bytes(input: Vec<u8>) -> Self {
        Self {
            // Z-TODO: Use more efficient library for deserialization.
            // https://github.com/rkyv/rkyv
            cache: bincode::deserialize(&input).unwrap(),
        }
    }
}

#[async_trait]
impl PreimageOracleClient for InMemoryOracle {
    async fn get(&self, key: PreimageKey) -> Result<Vec<u8>> {
        self.cache.get(&key).cloned().ok_or_else(|| anyhow!("Key not found in cache"))
    }

    async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> Result<()> {
        let value = self.cache.get(&key).ok_or_else(|| anyhow!("Key not found in cache"))?;
        buf.copy_from_slice(value.as_slice());
        Ok(())
    }
}

#[async_trait]
impl HintWriterClient for InMemoryOracle {
    async fn write(&self, _hint: &str) -> Result<()> {
        Ok(())
    }
}

impl InMemoryOracle {
    pub fn verify(&self) -> Result<()> {

        // TODO: Move all verification logic here.
        for (key, value) in self.cache.iter() {
            match key.key_type() {
                PreimageKeyType::Local => {
                    todo!();
                },
                PreimageKeyType::Keccak256 => {
                    let derived_key = PreimageKey::new(keccak256(value).into(), PreimageKeyType::Keccak256);
                    assert_eq!(*key, derived_key, "zkvm keccak constraint failed!");
                },
                PreimageKeyType::GlobalGeneric => {
                    unimplemented!();
                },
                PreimageKeyType::Sha256 => {
                    let mut derived_key: [u8; 32] = Sha256::digest(value).into();
                    derived_key[0] = 0x01; // VERSIONED_HASH_VERSION_KZG
                    assert_eq!(*key, derived_key, "zkvm sha256 constraint failed!");
                },
                PreimageKeyType::Blob => {
                    todo!();
                },
                PreimageKeyType::Precompile => {
                    let hint_data_key = PreimageKey::new(*key.into(), PreimageKeyType::Keccak256);
                    if let Some(hint_data) = self.cache.get(&hint_data_key) {
                        let precompile = Precompile::from_bytes(hint_data).unwrap();
                        let output = precompile.execute();
                        assert_eq!(value, output, "zkvm precompile constraint failed!")
                    } else {
                        anyhow!("precompile hint data not found");
                    }
                }
            }
        }

        Ok(())
    }
}
