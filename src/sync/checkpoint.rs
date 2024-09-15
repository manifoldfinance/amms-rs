use std::{
    fs::read_to_string,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy::{network::Network, providers::Provider, transports::Transport};

use indicatif::MultiProgress;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::Semaphore,
    task::{JoinHandle, JoinSet},
    time::sleep,
};

use crate::{
    amm::{
        factory::{AutomatedMarketMakerFactory, Factory},
        AMM,
    },
    errors::{AMMError, CheckpointError},
    filters, finish_progress, init_progress, update_progress_by_one,
};

// static TASK_PERMITS: Semaphore = Semaphore::const_new(100);

#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub timestamp: usize,
    pub block_number: u64,
    pub factories: Vec<Factory>,
    pub amms: Vec<AMM>,
}

impl Checkpoint {
    pub fn new(
        timestamp: usize,
        block_number: u64,
        factories: Vec<Factory>,
        amms: Vec<AMM>,
    ) -> Checkpoint {
        Checkpoint {
            timestamp,
            block_number,
            factories,
            amms,
        }
    }
}

// Get all pairs from last synced block and sync reserve values for each Dex in the `dexes` vec.
pub async fn sync_amms_from_checkpoint<T, N, P, A>(
    path_to_checkpoint: A,
    step: u64,
    provider: Arc<P>,
    max_concurrent_tasks: usize,
    delay_duration: Duration,
) -> Result<(Vec<Factory>, Vec<AMM>), AMMError>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N> + 'static,
    A: AsRef<Path>,
{
    tracing::info!("Syncing AMMs from checkpoint");
    let current_block = provider.get_block_number().await?;

    let checkpoint: Checkpoint =
        serde_json::from_str(read_to_string(&path_to_checkpoint)?.as_str())?;

    //
    tracing::info!("Checkpoint blocknumber: {:?}", checkpoint.block_number);
    tracing::info!("Current blocknumber: {:?}", current_block);

    // Create a semaphore to limit the number of concurrent tasks
    let semaphore: Arc<Semaphore> = Arc::new(Semaphore::new(max_concurrent_tasks));

    let mut aggregated_amms: Vec<AMM> = sync_amm_data_from_checkpoint(
        checkpoint.amms,
        checkpoint.block_number,
        current_block,
        provider.clone(),
        semaphore.clone(),
        delay_duration.clone(),
    )
    .await?;
    let factories = checkpoint.factories.clone();

    // Sync all pools from the since synced block
    aggregated_amms.extend(
        get_new_amms_from_range(
            factories,
            checkpoint.block_number,
            current_block,
            step,
            provider.clone(),
        )
        .await?,
    );

    //update the sync checkpoint
    construct_checkpoint(
        checkpoint.factories.clone(),
        &aggregated_amms,
        current_block,
        path_to_checkpoint,
    )?;

    Ok((checkpoint.factories, aggregated_amms))
}

pub async fn get_new_amms_from_range<T, N, P>(
    factories: Vec<Factory>,
    from_block: u64,
    to_block: u64,
    step: u64,
    provider: Arc<P>,
) -> Result<Vec<AMM>, AMMError>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N> + 'static,
{
    let mut new_amms = vec![];
    let mut join_set = JoinSet::new();
    tracing::info!("Getting new AMMs from range {} to {}", from_block, to_block);
    for factory in factories {
        let provider = provider.clone();

        // Spawn a new thread to get all pools and sync data for each dex
        join_set.spawn(async move {
            let mut amms = factory
                .get_all_pools_from_logs(from_block, to_block, step, provider.clone())
                .await?;

            factory
                .populate_amm_data(&mut amms, Some(to_block), provider.clone())
                .await?;

            // Clean empty pools
            amms = filters::filter_empty_amms(amms);

            Ok::<_, AMMError>(amms)
        });
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(result) => match result {
                Ok(amms) => {
                    new_amms.extend(amms);
                }
                Err(err) => return Err(err),
            },
            Err(err) => {
                return Err(AMMError::JoinError(err));
            }
        }
    }

    Ok(new_amms)
}

pub async fn sync_amm_data_from_checkpoint<T, N, P>(
    amms: Vec<AMM>,
    checkpoint_block: u64,
    to_block: u64,
    provider: Arc<P>,
    semaphore: Arc<Semaphore>,
    delay_duration: Duration,
) -> Result<Vec<AMM>, AMMError>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N> + 'static,
{
    let multi_progress: MultiProgress = MultiProgress::new();
    let progress: indicatif::ProgressBar = multi_progress.add(init_progress!(
        amms.len(),
        "Populating AMM Data from Checkpoint"
    ));
    progress.set_position(0);
    let mut synced_amms: Vec<AMM> = vec![];
    let mut join_set: JoinSet<Result<AMM, AMMError>> = JoinSet::new();

    for mut amm in amms {
        let middleware: Arc<P> = provider.clone();
        let semaphore_clone: Arc<Semaphore> = semaphore.clone();
        let delay = delay_duration;
        join_set.spawn(async move {
            // Acquire a permit before proceeding
            let _permit: tokio::sync::SemaphorePermit<'_> =
                semaphore_clone.acquire().await.unwrap();
            match amm {
                AMM::UniswapV2Pool(ref mut pool) => {
                    (pool.reserve_0, pool.reserve_1) =
                        pool.get_reserves(middleware, Some(to_block)).await?;
                }
                AMM::UniswapV3Pool(ref mut pool) => {
                    pool.populate_tick_data(checkpoint_block, middleware.clone())
                        .await?;
                    pool.sqrt_price = pool.get_sqrt_price(middleware.clone()).await?;
                    pool.liquidity = pool.get_liquidity(middleware.clone()).await?;
                }
                AMM::ERC4626Vault(ref mut vault) => {
                    (vault.vault_reserve, vault.asset_reserve) =
                        vault.get_reserves(middleware, Some(to_block)).await?;
                }
            }
            // Add a delay before the next task
            sleep(delay).await;
            Ok::<AMM, AMMError>(amm)
        });
    }

    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(result) => match result {
                Ok(amm) => {
                    update_progress_by_one!(progress);
                    synced_amms.push(amm);
                }
                Err(err) => return Err(err),
            },
            Err(err) => {
                tracing::error!(?err);
                return Err(AMMError::JoinError(err));
            }
        }
    }

    finish_progress!(progress);

    Ok(synced_amms)
}

pub fn sort_amms(amms: Vec<AMM>) -> (Vec<AMM>, Vec<AMM>, Vec<AMM>) {
    let mut uniswap_v2_pools = vec![];
    let mut uniswap_v3_pools = vec![];
    let mut erc_4626_vaults = vec![];
    for amm in amms {
        match amm {
            AMM::UniswapV2Pool(_) => uniswap_v2_pools.push(amm),
            AMM::UniswapV3Pool(_) => uniswap_v3_pools.push(amm),
            AMM::ERC4626Vault(_) => erc_4626_vaults.push(amm),
        }
    }

    (uniswap_v2_pools, uniswap_v3_pools, erc_4626_vaults)
}

pub async fn get_new_pools_from_range<T, N, P>(
    factories: Vec<Factory>,
    from_block: u64,
    to_block: u64,
    step: u64,
    provider: Arc<P>,
) -> Vec<JoinHandle<Result<Vec<AMM>, AMMError>>>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N> + 'static,
{
    // Create the filter with all the pair created events
    // Aggregate the populated pools from each thread
    let mut handles = vec![];

    for factory in factories {
        let provider = provider.clone();

        // Spawn a new thread to get all pools and sync data for each dex
        handles.push(tokio::spawn(async move {
            let mut pools = factory
                .get_all_pools_from_logs(from_block, to_block, step, provider.clone())
                .await?;

            factory
                .populate_amm_data(&mut pools, Some(to_block), provider.clone())
                .await?;

            // Clean empty pools
            pools = filters::filter_empty_amms(pools);

            Ok::<_, AMMError>(pools)
        }));
    }

    handles
}

pub fn construct_checkpoint<P>(
    factories: Vec<Factory>,
    amms: &[AMM],
    latest_block: u64,
    checkpoint_path: P,
) -> Result<(), CheckpointError>
where
    P: AsRef<Path>,
{
    let checkpoint = Checkpoint::new(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64() as usize,
        latest_block,
        factories,
        amms.to_vec(),
    );

    std::fs::write(checkpoint_path, serde_json::to_string_pretty(&checkpoint)?)?;

    Ok(())
}

// Deconstructs the checkpoint into a Vec<AMM>
pub fn deconstruct_checkpoint<P>(checkpoint_path: P) -> Result<(Vec<AMM>, u64), CheckpointError>
where
    P: AsRef<Path>,
{
    let checkpoint: Checkpoint = serde_json::from_str(read_to_string(checkpoint_path)?.as_str())?;
    Ok((checkpoint.amms, checkpoint.block_number))
}
