use crate::{
    amm::{factory::AutomatedMarketMakerFactory, AMM},
    errors::AMMError,
    finish_progress, init_progress, update_progress_by_one,
};
use alloy::{
    network::Network,
    primitives::{address, Address, B256, U256},
    providers::Provider,
    rpc::types::eth::Log,
    sol,
    sol_types::SolEvent,
    transports::Transport,
};
use async_trait::async_trait;
use futures::future::join_all;
use indicatif::MultiProgress;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use tokio::{task::JoinError, time::sleep};
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::Retry;
use tracing::instrument;

use super::{batch_request, UniswapV2Pool, U256_1};

sol! {
    /// Interface of the UniswapV2Factory contract
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniswapV2Factory {
        event PairCreated(address indexed token0, address indexed token1, address pair, uint256 index);
        function getPair(address tokenA, address tokenB) external view returns (address pair);
        function allPairs(uint256 index) external view returns (address pair);
        function allPairsLength() external view returns (uint256 length);
    }
}

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
pub struct UniswapV2Factory {
    pub address: Address,
    pub creation_block: u64,
    pub fee: u32,
}

impl UniswapV2Factory {
    pub fn new(address: Address, creation_block: u64, fee: u32) -> UniswapV2Factory {
        UniswapV2Factory {
            address,
            creation_block,
            fee,
        }
    }

    pub async fn get_all_pairs_via_batched_calls<T, N, P>(
        &self,
        provider: Arc<P>,
    ) -> Result<Vec<AMM>, AMMError>
    where
        T: Transport + Clone,
        N: Network,
        P: Provider<T, N>,
    {
        let factory = IUniswapV2Factory::new(self.address, provider.clone());

        let IUniswapV2Factory::allPairsLengthReturn {
            length: pairs_length,
        } = factory.allPairsLength().call().await?;

        let mut pairs = vec![];
        // NOTE: max batch size for this call until codesize is too large
        let step = 766;
        let mut idx_from = U256::ZERO;
        let mut idx_to = if step > pairs_length.to::<usize>() {
            pairs_length
        } else {
            U256::from(step)
        };
        let range = (0..pairs_length.to::<usize>()).step_by(step);
        let multi_progress = MultiProgress::new();
        let progress = multi_progress.add(init_progress!(range.clone().count(), "Getting AMMs v2"));
        progress.set_position(0);

        for _ in range {
            update_progress_by_one!(progress);
            pairs.append(
                &mut batch_request::get_pairs_batch_request(
                    self.address,
                    idx_from,
                    idx_to,
                    provider.clone(),
                )
                .await?,
            );

            idx_from = idx_to;

            if idx_to + U256::from(step) > pairs_length {
                idx_to = pairs_length - U256_1
            } else {
                idx_to += U256::from(step);
            }
        }

        let mut amms = vec![];

        // Create new empty pools for each pair
        for addr in pairs {
            let amm = UniswapV2Pool {
                address: addr,
                ..Default::default()
            };

            amms.push(AMM::UniswapV2Pool(amm));
        }
        finish_progress!(progress);

        Ok(amms)
    }

    pub async fn safe_amms<T, N, P>(
        &self,
        provider: Arc<P>,
        safe_tokens: Vec<Address>,
    ) -> Result<Vec<AMM>, AMMError>
    where
        T: Transport + Clone,
        N: Network,
        P: Provider<T, N> + 'static,
    {
        let factory: IUniswapV2Factory::IUniswapV2FactoryInstance<T, Arc<P>, N> =
            IUniswapV2Factory::new(self.address, provider.clone());
        let mut amms: Vec<AMM> = vec![];

        let multi_progress: MultiProgress = MultiProgress::new();
        let progress: indicatif::ProgressBar =
            multi_progress.add(init_progress!(safe_tokens.clone().len(), "Getting AMMs v2"));
        progress.set_position(0);

        let mut futures: Vec<tokio::task::JoinHandle<Result<Option<AMM>, _>>> = Vec::new();

        let retry_strategy = ExponentialBackoff::from_millis(500).map(jitter).take(5);

        for (i, token_a) in safe_tokens.iter().enumerate() {
            update_progress_by_one!(progress);

            // Collect async tasks for all pairs
            for token_b in safe_tokens.iter().skip(i + 1) {
                let factory_clone: IUniswapV2Factory::IUniswapV2FactoryInstance<T, Arc<P>, N> =
                    factory.clone();
                let token_a: Address = *token_a;
                let token_b: Address = *token_b;

                // Clone retry_strategy here so it is not moved into the async block
                let retry_strategy = retry_strategy.clone();

                futures.push(tokio::spawn(async move {
                    // Rate limit - introduce a delay between each `getPair` call
                    sleep(Duration::from_millis(500)).await; // 500ms delay

                    // let result: Result<IUniswapV2Factory::getPairReturn, alloy::contract::Error> =
                    //     factory_clone.getPair(token_a, token_b).call().await;
                    // Use retry logic for each getPair call
                    let result: Result<IUniswapV2Factory::getPairReturn, alloy::contract::Error> =
                        Retry::spawn(retry_strategy.clone(), || async {
                            factory_clone.getPair(token_a, token_b).call().await
                        })
                        .await;
                    match result {
                        Ok(IUniswapV2Factory::getPairReturn { pair: pair_address })
                            if pair_address
                                != address!("0000000000000000000000000000000000000000") =>
                        {
                            // Return AMM if valid
                            Ok(Some(AMM::UniswapV2Pool(UniswapV2Pool {
                                address: pair_address,
                                ..Default::default()
                            })))
                        }
                        Ok(_) => Ok(None), // Return None if pair address is invalid
                        Err(e) => Err(e),  // Propagate any errors
                    }
                }));
            }
        }

        // Await all tasks concurrently
        let results: Vec<Result<Result<Option<AMM>, _>, JoinError>> = join_all(futures).await;

        // Process results, flatten the Vec<Option<AMM>>, and handle errors
        for result in results {
            match result {
                Ok(Ok(Some(amm))) => amms.push(amm), // Collect valid AMMs
                Ok(Ok(None)) => {}                   // Skip None results
                Ok(Err(e)) => return Err(e.into()),  // Propagate factory call errors
                Err(join_err) => return Err(join_err.into()), // Handle tokio::spawn errors
            }
        }

        finish_progress!(progress);

        Ok(amms)
    }
}

#[async_trait]
impl AutomatedMarketMakerFactory for UniswapV2Factory {
    fn address(&self) -> Address {
        self.address
    }

    fn amm_created_event_signature(&self) -> B256 {
        IUniswapV2Factory::PairCreated::SIGNATURE_HASH
    }

    async fn new_amm_from_log<T, N, P>(&self, log: Log, provider: Arc<P>) -> Result<AMM, AMMError>
    where
        T: Transport + Clone,
        N: Network,
        P: Provider<T, N>,
    {
        let pair_created_event = IUniswapV2Factory::PairCreated::decode_log(log.as_ref(), true)?;
        Ok(AMM::UniswapV2Pool(
            UniswapV2Pool::new_from_address(pair_created_event.pair, self.fee, provider).await?,
        ))
    }

    fn new_empty_amm_from_log(&self, log: Log) -> Result<AMM, alloy::sol_types::Error> {
        let pair_created_event = IUniswapV2Factory::PairCreated::decode_log(log.as_ref(), true)?;

        Ok(AMM::UniswapV2Pool(UniswapV2Pool {
            address: pair_created_event.pair,
            token_a: pair_created_event.token0,
            token_b: pair_created_event.token1,
            token_a_decimals: 0,
            token_b_decimals: 0,
            reserve_0: 0,
            reserve_1: 0,
            fee: 0,
        }))
    }

    #[instrument(skip(self, middleware) level = "debug")]
    async fn get_all_amms<T, N, P>(
        &self,
        _to_block: Option<u64>,
        middleware: Arc<P>,
        _step: u64,
    ) -> Result<Vec<AMM>, AMMError>
    where
        T: Transport + Clone,
        N: Network,
        P: Provider<T, N>,
    {
        self.get_all_pairs_via_batched_calls(middleware).await
    }

    async fn get_safe_amms<T, N, P>(
        &self,
        middleware: Arc<P>,
        safe_tokens: Vec<Address>,
    ) -> Result<Vec<AMM>, AMMError>
    where
        T: Transport + Clone,
        N: Network,
        P: Provider<T, N> + 'static,
    {
        self.safe_amms(middleware.clone(), safe_tokens).await
    }

    async fn populate_amm_data<T, N, P>(
        &self,
        amms: &mut [AMM],
        _block_number: Option<u64>,
        middleware: Arc<P>,
    ) -> Result<(), AMMError>
    where
        T: Transport + Clone,
        N: Network,
        P: Provider<T, N>,
    {
        // Max batch size for call
        let step = 127;
        for amm_chunk in amms.chunks_mut(step) {
            batch_request::get_amm_data_batch_request(amm_chunk, middleware.clone()).await?;
        }
        Ok(())
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}
