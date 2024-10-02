#![doc = include_str!("../README.md")]
#![warn(missing_debug_implementations, missing_docs, unreachable_pub, rustdoc::all)]
#![deny(unused_must_use, rust_2018_idioms)]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![no_std]
#![cfg_attr(any(target_arch = "mips", target_arch = "riscv64"), no_main)]

extern crate alloc;

use alloc::sync::Arc;
use kona_client::{
    l1::{DerivationDriver, OracleBlobProvider, OracleL1ChainProvider},
    l2::OracleL2ChainProvider,
    BootInfo, CachingOracle,
};
use kona_common_proc::client_entry;

pub(crate) mod fault;
use fault::{fpvm_handle_register, HINT_WRITER, ORACLE_READER};
use tokio::sync::RwLock;

/// The size of the LRU cache in the oracle.
const ORACLE_LRU_SIZE: usize = 1024;

#[client_entry(100_000_000)]
fn main() -> Result<()> {
    #[cfg(feature = "tracing-subscriber")]
    {
        use anyhow::anyhow;
        use tracing::Level;

        let subscriber = tracing_subscriber::fmt().with_max_level(Level::DEBUG).finish();
        tracing::subscriber::set_global_default(subscriber).map_err(|e| anyhow!(e))?;
    }

    kona_common::block_on(async move {
        ////////////////////////////////////////////////////////////////
        //                          PROLOGUE                          //
        ////////////////////////////////////////////////////////////////

        let mut oracle = CachingOracle::new(ORACLE_LRU_SIZE, ORACLE_READER, HINT_WRITER);
        let arc_oracle = Arc::new(RwLock::new(oracle));
        let boot = Arc::new(BootInfo::load(arc_oracle.get_mut()).await?);
        let l1_provider = OracleL1ChainProvider::new(boot.clone(), arc_oracle.clone());
        let l2_provider = OracleL2ChainProvider::new(boot.clone(), arc_oracle.clone());
        let beacon = OracleBlobProvider::new(arc_oracle);

        ////////////////////////////////////////////////////////////////
        //                   DERIVATION & EXECUTION                   //
        ////////////////////////////////////////////////////////////////

        // Create a new derivation driver with the given boot information and oracle.
        let mut driver = DerivationDriver::new(
            boot.as_ref(),
            &mut oracle,
            beacon,
            l1_provider,
            l2_provider.clone(),
        )
        .await?;

        // Run the derivation pipeline until we are able to produce the output root of the claimed
        // L2 block.
        let (number, output_root) = driver
            .produce_output(&boot.rollup_config, &l2_provider, &l2_provider, fpvm_handle_register)
            .await?;

        ////////////////////////////////////////////////////////////////
        //                          EPILOGUE                          //
        ////////////////////////////////////////////////////////////////

        if number != boot.l2_claim_block || output_root != boot.l2_claim {
            tracing::error!(
                target: "client",
                "Failed to validate L2 block #{number} with output root {output_root}",
                number = number,
                output_root = output_root
            );
            kona_common::io::print(&alloc::format!(
                "Failed to validate L2 block #{} with output root {}\n",
                number,
                output_root
            ));

            kona_common::io::exit(1);
        }

        tracing::info!(
            target: "client",
            "Successfully validated L2 block #{number} with output root {output_root}",
            number = number,
            output_root = output_root
        );

        kona_common::io::print(&alloc::format!(
            "Successfully validated L2 block #{} with output root {}\n",
            number,
            output_root
        ));

        Ok::<_, anyhow::Error>(())
    })
}
