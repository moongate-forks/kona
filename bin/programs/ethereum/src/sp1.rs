use crate::{BlockWithSenders, Header, InputFetcher, B256, U256};
pub use ZkvmTrieDBHinter as TrieDBHinter;

pub struct InputFetcherImpl {}

impl InputFetcher for InputFetcherImpl {
    fn new() -> Self {
        InputFetcherImpl {}
    }

    fn get_block_with_senders(&self, block_number: U256) -> BlockWithSenders {
        // Read from sp1_zkvm::io
        todo!();
    }

    fn header_by_hash(&self, hash: B256) -> Result<Header> {
        // Read from sp1_zkvm::io
        todo!();
    }
}

pub struct TrieDBFetcherImpl(ZkvmTrieDBFetcher);

impl TrieDBFetcherImpl {
    pub fn new() -> Self {
        let inner_fetcher = sp1_zkvm::io::read();
        TrieDBFetcherImpl(inner_fetcher)
    }
}

impl TrieDBFetcher for TrieDBFetcherImpl {
    fn trie_node_preimage(&self, key: B256) -> Result<Bytes> {
        self.0.trie_node_preimage(key)
    }

    fn bytecode_by_hash(&self, hash: B256) -> Result<Bytes> {
        self.0.bytecode_by_hash(hash)
    }

    fn header_by_hash(&self, hash: B256) -> Result<Bytes> {
        self.0.header_by_hash(hash)
    }
}
