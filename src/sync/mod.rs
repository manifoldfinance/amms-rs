pub mod checkpoint;

use crate::update_progress_by_one;
use crate::{
    amm::{
        factory::{AutomatedMarketMakerFactory, Factory},
        uniswap_v2, uniswap_v3, AutomatedMarketMaker, AMM,
    },
    errors::AMMError,
    filters, finish_progress, init_progress,
    sync::checkpoint::sync_amms_from_checkpoint,
};
use alloy::primitives::Address;
use alloy::{network::Network, providers::Provider, transports::Transport};
use indicatif::MultiProgress;
use std::time::Duration;
use std::{panic::resume_unwind, sync::Arc};

/// Syncs all AMMs from the supplied factories.
///
/// factories - A vector of factories to sync AMMs from.
/// provider - A provider to use for syncing AMMs.
/// checkpoint_path - A path to save a checkpoint of the synced AMMs.
/// step - The step size for batched RPC requests.
/// Returns a tuple of the synced AMMs and the last synced block number.
pub async fn sync_amms<T, N, P>(
    factories: Vec<Factory>,
    provider: Arc<P>,
    checkpoint_path: Option<&str>,
    step: u64,
    max_concurrent_tasks: usize,
    delay_duration: Duration,
) -> Result<(Vec<AMM>, u64), AMMError>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N> + 'static,
{
    tracing::info!(?step, ?factories, "Syncing AMMs");

    let current_block: u64 = provider.get_block_number().await?;

    // read checkpoint file if exists
    if let Some(checkpoint_path) = checkpoint_path {
        if std::fs::File::open(checkpoint_path).is_ok() {
            let (_, amms) = sync_amms_from_checkpoint(
                checkpoint_path,
                step,
                provider,
                max_concurrent_tasks,
                delay_duration,
            )
            .await?;
            return Ok((amms, current_block));
        }
    }

    // Aggregate the populated pools from each thread
    let mut aggregated_amms: Vec<AMM> = vec![];
    let mut handles = vec![];

    // For each dex supplied, get all pair created events and get reserve values
    for factory in factories.clone() {
        let provider = provider.clone();

        // Spawn a new thread to get all pools and sync data for each dex
        handles.push(tokio::spawn(async move {
            tracing::info!(?factory, "Getting all AMMs from factory");
            // Get all of the amms from the factory
            let mut amms: Vec<AMM> = factory
                .get_all_amms(Some(current_block), provider.clone(), step)
                .await?;

            tracing::info!(?factory, "Populating AMMs from factory");
            populate_amms(&mut amms, current_block, provider.clone()).await?;

            // Clean empty pools
            amms = filters::filter_empty_amms(amms);

            // If the factory is UniswapV2, set the fee for each pool according to the factory fee
            if let Factory::UniswapV2Factory(factory) = factory {
                for amm in amms.iter_mut() {
                    if let AMM::UniswapV2Pool(ref mut pool) = amm {
                        pool.fee = factory.fee;
                    }
                }
            }

            Ok::<_, AMMError>(amms)
        }));
    }

    for handle in handles {
        match handle.await {
            Ok(sync_result) => aggregated_amms.extend(sync_result?),
            Err(err) => {
                {
                    if err.is_panic() {
                        // Resume the panic on the main task
                        resume_unwind(err.into_panic());
                    }
                }
            }
        }
    }

    // Save a checkpoint if a path is provided

    if let Some(checkpoint_path) = checkpoint_path {
        checkpoint::construct_checkpoint(
            factories,
            &aggregated_amms,
            current_block,
            checkpoint_path,
        )?;
    }

    // Return the populated aggregated amms vec
    Ok((aggregated_amms, current_block))
}

/// Syncs safe AMMs from the supplied factories.
///
/// factories - A vector of factories to sync AMMs from.
/// provider - A provider to use for syncing AMMs.
/// safe_tokens - List of safe token addresses
/// Returns a tuple of the synced AMMs and the last synced block number.
pub async fn sync_safe_amms<T, N, P>(
    factories: Vec<Factory>,
    provider: Arc<P>,
    safe_tokens: Vec<Address>,
) -> Result<(Vec<AMM>, u64), AMMError>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N> + 'static,
{
    tracing::info!(?safe_tokens, ?factories, "Syncing AMMs");

    let current_block: u64 = provider.get_block_number().await?;

    // Aggregate the populated pools from each thread
    let mut aggregated_amms: Vec<AMM> = vec![];
    let mut handles = vec![];

    // For each dex supplied, get all pair created events and get reserve values
    for factory in factories.clone() {
        let provider: Arc<P> = provider.clone();
        let safe_tokens: Vec<Address> = safe_tokens.clone();

        // Spawn a new thread to get all pools and sync data for each dex
        handles.push(tokio::spawn(async move {
            tracing::info!(?factory, "Getting all AMMs from factory");
            // Get all of the amms from the factory
            let mut amms: Vec<AMM> = factory
                .get_safe_amms(provider.clone(), safe_tokens.clone())
                .await?;

            tracing::info!(?factory, "Populating AMMs from factory");
            populate_amms(&mut amms, current_block, provider.clone()).await?;

            // Clean empty pools
            amms = filters::filter_empty_amms(amms);

            // If the factory is UniswapV2, set the fee for each pool according to the factory fee
            if let Factory::UniswapV2Factory(factory) = factory {
                for amm in amms.iter_mut() {
                    if let AMM::UniswapV2Pool(ref mut pool) = amm {
                        pool.fee = factory.fee;
                    }
                }
            }

            Ok::<_, AMMError>(amms)
        }));
    }

    for handle in handles {
        match handle.await {
            Ok(sync_result) => aggregated_amms.extend(sync_result?),
            Err(err) => {
                {
                    if err.is_panic() {
                        // Resume the panic on the main task
                        resume_unwind(err.into_panic());
                    }
                }
            }
        }
    }

    // Return the populated aggregated amms vec
    Ok((aggregated_amms, current_block))
}

pub fn amms_are_congruent(amms: &[AMM]) -> bool {
    let expected_amm = &amms[0];

    for amm in amms {
        if std::mem::discriminant(expected_amm) != std::mem::discriminant(amm) {
            return false;
        }
    }
    true
}

// Gets all pool data and sync reserves
pub async fn populate_amms<T, N, P>(
    amms: &mut [AMM],
    block_number: u64,
    provider: Arc<P>,
) -> Result<(), AMMError>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N>,
{
    if amms_are_congruent(amms) {
        let multi_progress = MultiProgress::new();
        match amms[0] {
            AMM::UniswapV2Pool(_) => {
                // Max batch size for call
                let step = 127;
                let amm_chunks = amms.chunks_mut(step);
                let progress =
                    multi_progress.add(init_progress!(amm_chunks.len(), "Populating AMM data v2"));
                progress.set_position(0);
                for amm_chunk in amm_chunks {
                    update_progress_by_one!(progress);
                    uniswap_v2::batch_request::get_amm_data_batch_request(
                        amm_chunk,
                        provider.clone(),
                    )
                    .await?;
                }
                finish_progress!(progress);
            }

            AMM::UniswapV3Pool(_) => {
                // Max batch size for call
                let step = 76;
                let amm_chunks = amms.chunks_mut(step);
                let progress =
                    multi_progress.add(init_progress!(amm_chunks.len(), "Populating AMM data v3"));
                progress.set_position(0);
                for amm_chunk in amm_chunks {
                    update_progress_by_one!(progress);
                    uniswap_v3::batch_request::get_amm_data_batch_request(
                        amm_chunk,
                        block_number,
                        provider.clone(),
                    )
                    .await?;
                }
                finish_progress!(progress);
            }

            // TODO: Implement batch request
            AMM::ERC4626Vault(_) => {
                let progress =
                    multi_progress.add(init_progress!(amms.len(), "Populating ERC4626 vaults"));
                progress.set_position(0);
                for amm in amms {
                    update_progress_by_one!(progress);
                    amm.populate_data(None, provider.clone()).await?;
                }
                finish_progress!(progress);
            }
        }
    } else {
        return Err(AMMError::IncongruentAMMs);
    }

    // For each pair in the pairs vec, get the pool data
    Ok(())
}
