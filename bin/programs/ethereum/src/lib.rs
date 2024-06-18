pub(crate) use alloy_consensus::Header;
use anyhow::Result;
use cfg_if::cfg_if;
pub(crate) use reth_primitives::{BlockWithSenders, B256, U256};

cfg_if! {
    if #[cfg(target_os = "zkvm")] {
        #[doc = "Concrete implementation of the [BasicKernelInterface] trait for the `zkvm` target architecture."]
        mod sp1;
        pub use sp1::InputFetcherImpl;
        pub use sp1::TrieDBFetcherImpl;
        pub use sp1::TrieDBHinter;
    } else {
        #[doc = "Concrete implementation of the [BasicKernelInterface] trait for the `native` target architecture."]
        mod kona;
        pub use kona::InputFetcherImpl;
        pub use kona::TrieDBFetcherImpl;
        pub use kona::TrieDBHinter;
    }
}

pub trait InputFetcher {
    fn new() -> Self;
    fn get_block_with_senders(&self, block_number: u64) -> Result<BlockWithSenders>;
    fn header_by_hash(&self, hash: B256) -> Result<Header>;
}
