use std::sync::Arc;

use alloy::{
    primitives::{address, Address},
    providers::ProviderBuilder,
};

use amms::{
    amm::{
        factory::Factory, uniswap_v2::factory::UniswapV2Factory,
        uniswap_v3::factory::UniswapV3Factory,
    },
    sync,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    // Add rpc endpoint here:
    let rpc_endpoint: String = std::env::var("ETHEREUM_RPC_ENDPOINT")?;
    let provider: Arc<
        alloy::providers::RootProvider<alloy::transports::http::Http<reqwest::Client>>,
    > = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse()?));

    let factories: Vec<Factory> = vec![
        // Add UniswapV2
        Factory::UniswapV2Factory(UniswapV2Factory::new(
            address!("5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f"),
            2638438,
            300,
        )),
        // Add Sushiswap
        Factory::UniswapV2Factory(UniswapV2Factory::new(
            address!("C0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac"),
            10794229,
            300,
        )),
        // Add UniswapV3
        // Factory::UniswapV3Factory(UniswapV3Factory::new(
        //     address!("1F98431c8aD98523631AE4a59f267346ea31F984"),
        //     12369621,
        // )),
    ];
    let safe_tokens: Vec<Address> = vec![
        address!("dAC17F958D2ee523a2206206994597C13D831ec7"), // USDT
        address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"), // WBTC
        address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
    ];

    // Sync pairs
    sync::sync_safe_amms(factories, provider, safe_tokens).await?;

    Ok(())
}
