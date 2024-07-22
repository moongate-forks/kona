//! This module contains all CLI-specific code for the host binary.

use crate::{
    fetcher::ThreadSafeFetcher,
    kv::{
        DiskKeyValueStore, LocalKeyValueStore, MemoryKeyValueStore, SharedKeyValueStore,
        SplitKeyValueStore,
    },
    util, DefaultFetcher,
};
use alloy_primitives::B256;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clap::{ArgAction, Parser};
use kona_derive::online::{OnlineBeaconClient, OnlineBlobProvider};
use serde::Serialize;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::RwLock;

mod parser;
pub(crate) use parser::parse_b256;

mod tracing_util;
pub use tracing_util::init_tracing_subscriber;

/// A trait for the host binary CLI application arguments.
#[async_trait]
pub trait HostCliTrait {
    /// Returns `true` if the host is running in offline mode.
    fn is_offline(&self) -> bool;

    /// Returns the path of the client binary to execute.
    fn exec(&self) -> Option<String>;

    /// Returns an instance of a [SharedKeyValueStore], accoring to the CLI arguments.
    fn construct_kv_store(&self) -> SharedKeyValueStore;

    /// Returns an instance of a [ThreadSafeFetcher], according to the CLI arguments.
    async fn construct_fetcher(&self) -> Result<Option<ThreadSafeFetcher>>;
}

/// The host binary CLI application arguments.
#[derive(Parser, Serialize, Clone, Debug)]
pub struct HostCli {
    /// Verbosity level (0-4)
    #[arg(long, short, help = "Verbosity level (0-4)", action = ArgAction::Count)]
    pub v: u8,
    /// Hash of the L1 head block. Derivation stops after this block is processed.
    #[clap(long, value_parser = parse_b256)]
    pub l1_head: B256,
    /// Hash of the L2 block at the L2 Output Root.
    #[clap(long, value_parser = parse_b256)]
    pub l2_head: B256,
    /// Agreed L2 Output Root to start derivation from.
    #[clap(long, value_parser = parse_b256)]
    pub l2_output_root: B256,
    /// Claimed L2 output root to validate
    #[clap(long, value_parser = parse_b256)]
    pub l2_claim: B256,
    /// Number of the L2 block that the claim is from.
    #[clap(long)]
    pub l2_block_number: u64,
    /// The L2 chain ID.
    #[clap(long)]
    pub l2_chain_id: u64,
    /// Address of L2 JSON-RPC endpoint to use (eth and debug namespace required).
    #[clap(long)]
    pub l2_node_address: Option<String>,
    /// Address of L1 JSON-RPC endpoint to use (eth namespace required)
    #[clap(long)]
    pub l1_node_address: Option<String>,
    /// Address of the L1 Beacon API endpoint to use.
    #[clap(long)]
    pub l1_beacon_address: Option<String>,
    /// The Data Directory for preimage data storage. Default uses in-memory storage.
    #[clap(long)]
    pub data_dir: Option<PathBuf>,
    /// Run the specified client program as a separate process detached from the host. Default is
    /// to run the client program in the host process.
    #[clap(long)]
    pub exec: Option<String>,
    /// Run in pre-image server mode without executing any client program. Defaults to `false`.
    #[clap(long)]
    pub server: bool,
}

#[async_trait]
impl HostCliTrait for HostCli {
    /// Returns `true` if the host is running in offline mode.
    fn is_offline(&self) -> bool {
        self.l1_node_address.is_none() ||
            self.l2_node_address.is_none() ||
            self.l1_beacon_address.is_none()
    }

    fn exec(&self) -> Option<String> {
        self.exec.clone()
    }

    /// Parses the CLI arguments and returns a new instance of a [SharedKeyValueStore], as it is
    /// configured to be created.
    fn construct_kv_store(&self) -> SharedKeyValueStore {
        let local_kv_store = LocalKeyValueStore::new(self.clone());

        let kv_store: SharedKeyValueStore = if let Some(ref data_dir) = self.data_dir {
            let disk_kv_store = DiskKeyValueStore::new(data_dir.clone());
            let split_kv_store = SplitKeyValueStore::new(local_kv_store, disk_kv_store);
            Arc::new(RwLock::new(split_kv_store))
        } else {
            let mem_kv_store = MemoryKeyValueStore::new();
            let split_kv_store = SplitKeyValueStore::new(local_kv_store, mem_kv_store);
            Arc::new(RwLock::new(split_kv_store))
        };

        kv_store
    }

    /// Parses the CLI arguments and returns a new instance of a [ThreadSafeFetcher].
    async fn construct_fetcher(&self) -> Result<Option<ThreadSafeFetcher>> {
        let fetcher: Option<ThreadSafeFetcher> = if !self.is_offline() {
            let kv_store = self.construct_kv_store();
            let beacon_client = OnlineBeaconClient::new_http(
                self.l1_beacon_address.clone().expect("Beacon API URL must be set"),
            );
            let mut blob_provider = OnlineBlobProvider::new(beacon_client, None, None);
            blob_provider
                .load_configs()
                .await
                .map_err(|e| anyhow!("Failed to load blob provider configuration: {e}"))?;
            let l1_provider =
                util::http_provider(self.l1_node_address.as_ref().expect("Provider must be set"));
            let l2_provider =
                util::http_provider(self.l2_node_address.as_ref().expect("Provider must be set"));
            Some(Arc::new(RwLock::new(DefaultFetcher::new(
                kv_store,
                l1_provider,
                blob_provider,
                l2_provider,
                self.l2_head,
            ))))
        } else {
            None
        };
        Ok(fetcher)
    }
}
