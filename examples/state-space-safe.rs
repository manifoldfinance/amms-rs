use std::{sync::Arc, time::Duration};

use alloy::{
    primitives::{address, Address},
    providers::ProviderBuilder,
    rpc::client::WsConnect,
};

use amms::{
    amm::{factory::Factory, uniswap_v2::factory::UniswapV2Factory, AMM},
    discovery,
    state_space::StateSpaceManager,
    sync,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let ws_endpoint = std::env::var("ETHEREUM_WS_ENDPOINT")?;

    // Initialize WS provider
    let ws = WsConnect::new(ws_endpoint);
    let provider = Arc::new(ProviderBuilder::new().on_ws(ws).await?);

    // Initialize factories
    let factories = vec![
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
    ];

    let safe_tokens: Vec<Address> = vec![
        address!("dAC17F958D2ee523a2206206994597C13D831ec7"), // USDT
        address!("2260FAC5E5542a773Aa44fBCfeDf7C193bc2C599"), // WBTC
        address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), // WETH
    ];

    // Sync amms
    let (amms, last_synced_block) =
        sync::sync_safe_amms(factories, provider.clone(), safe_tokens).await?;

    // Initialize state space manager
    let state_space_manager = StateSpaceManager::new(amms, last_synced_block, 100, 100, provider);

    //Listen for state changes and print them out
    let (mut rx, _join_handles) = state_space_manager.subscribe_state_changes().await?;

    for _ in 0..10 {
        if let Some(state_changes) = rx.recv().await {
            println!("State changes: {:?}", state_changes);
        }
    }

    Ok(())
}
