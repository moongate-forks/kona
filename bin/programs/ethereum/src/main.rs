//! A program to verify a Ethereum STF in the zkVM using the Kona DB and the Reth Ethereum block
//! executor.
#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![no_std]
#![cfg_attr(any(target_arch = "mips", target_arch = "riscv64"), no_main)]

extern crate alloc;

use alloy_consensus::Sealable;
use ethereum_program::{InputFetcher, InputFetcherImpl, TrieDBFetcherImpl, TrieDBHinter};
use kona_common_proc::client_entry;
use kona_mpt::TrieDB;
use reth_evm::execute::{BlockExecutorProvider, Executor, ProviderError};
use reth_evm_ethereum::execute::EthExecutorProvider;
use reth_primitives::{Address, B256, U256};
use reth_revm::Database;
use revm::{
    primitives::{AccountInfo, Bytecode},
    Database as RevmDatabase,
};

#[client_entry(0x77359400)]
pub fn main() -> Result<()> {
    // TODO: hardcoding the block number for now, in the future we can also fetch this either from a
    // `BootInfo`-like struct or from the zkVM input.
    let block_number: u64 = 123;

    let trie_db_fetcher = TrieDBFetcherImpl::new();
    let trie_db_hinter = TrieDBHinter;
    let input_fetcher = InputFetcherImpl::new();

    let block_with_senders = input_fetcher
        .get_block_with_senders(block_number)
        .expect("Failed to get block with senders");
    let parent_header = input_fetcher
        .header_by_hash(block_with_senders.header.parent_hash)
        .expect("Failed to get parent header");

    let total_difficulty = U256::ZERO; // TODO: change this to be correct?

    let sealed_header = parent_header.clone().seal_slow();
    let trie_db =
        TrieDB::new(parent_header.state_root, sealed_header, trie_db_fetcher, trie_db_hinter);
    let wrapper = Wrapper(trie_db);
    let executor = EthExecutorProvider::mainnet().executor(wrapper);
    let output = executor.execute((&block_with_senders, total_difficulty).into()).unwrap();

    // TODO: given the `output`, compute the new state root and the new header.
    Ok::<_, anyhow::Error>(())
}

struct Wrapper<F: kona_mpt::TrieDBFetcher, H: kona_mpt::TrieDBHinter>(TrieDB<F, H>);

impl<F: kona_mpt::TrieDBFetcher, H: kona_mpt::TrieDBHinter> Database for Wrapper<F, H> {
    type Error = ProviderError;
    fn basic(
        &mut self,
        address: Address,
    ) -> Result<Option<reth_primitives::revm_primitives::AccountInfo>, Self::Error> {
        self.0
            .basic(address)
            .map_err(|_| ProviderError::UnsupportedProvider)
            .map(|r| r.map(convert_account))
    }

    fn block_hash(&mut self, number: U256) -> Result<B256, Self::Error> {
        self.0.block_hash(number).map_err(|_| ProviderError::UnsupportedProvider)
    }

    fn code_by_hash(
        &mut self,
        code_hash: B256,
    ) -> Result<reth_primitives::revm_primitives::Bytecode, Self::Error> {
        self.0
            .code_by_hash(code_hash)
            .map_err(|_| ProviderError::UnsupportedProvider)
            .map(convert_bytecode)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.0.storage(address, index).map_err(|_| ProviderError::UnsupportedProvider)
    }
}

fn convert_account(account: AccountInfo) -> reth_primitives::revm_primitives::AccountInfo {
    reth_primitives::revm_primitives::AccountInfo {
        nonce: account.nonce,
        balance: account.balance,
        code_hash: account.code_hash,
        code: None,
    }
}

fn convert_bytecode(bytecode: Bytecode) -> reth_primitives::revm_primitives::Bytecode {
    let as_vec = bytecode.original_byte_slice().to_vec();
    reth_primitives::revm_primitives::Bytecode::new_raw(as_vec.into())
}
