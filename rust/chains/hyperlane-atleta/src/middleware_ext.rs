use async_trait::async_trait;
use ethers::providers::Middleware;
use ethers::types::{BlockNumber, U64};

pub const BLOCK_ERROR_MSG: &str = "Unable to get finalized block number";

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait MiddlewareExt: Middleware {
    async fn get_finalized_block_number(&self) -> Result<Option<U64>, Self::Error> {
        self.get_block(BlockNumber::Finalized)
            .await
            .map(|block| block.unwrap_or_default().number)
    }
}

impl<T: Middleware> MiddlewareExt for T {}
