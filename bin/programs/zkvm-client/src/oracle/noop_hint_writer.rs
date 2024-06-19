use kona_preimage::HintWriterClient;
use async_trait::async_trait;
use anyhow::Result;
use alloc::boxed::Box;

pub struct NoopHintWriter;

#[async_trait]
impl HintWriterClient for NoopHintWriter {
    async fn write(&self, _hint: &str) -> Result<()> {
        Ok(())
    }
}
