// Copyright 2020 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use async_std::fs::File;
use async_std::io::BufReader;
use blocks::{BlockHeader, Tipset, TipsetKeys};
use chain::ChainStore;
use cid::Cid;
use encoding::Cbor;
use fil_types::verifier::ProofVerifier;
use forest_car::load_car;
use futures::AsyncRead;
use ipld_blockstore::BlockStore;
use log::{debug, info};
use net_utils::FetchProgress;
use state_manager::StateManager;
use std::convert::TryFrom;
use std::error::Error as StdError;
use std::include_bytes;
use std::sync::Arc;
use url::Url;

#[cfg(feature = "testing")]
pub const EXPORT_SR_40: &[u8; 1226395] = include_bytes!("mainnet/export40.car");

/// Uses an optional file path or the default genesis to parse the genesis and determine if
/// chain store has existing data for the given genesis.
pub async fn initialize_genesis<BS>(
    genesis_fp: Option<&String>,
    state_manager: &StateManager<BS>,
) -> Result<(Tipset, String), Box<dyn StdError>>
where
    BS: BlockStore + Send + Sync + 'static,
{
    let genesis = match genesis_fp {
        Some(path) => {
            let file = File::open(path).await?;
            let reader = BufReader::new(file);
            process_car(reader, state_manager.chain_store()).await?
        }
        None => {
            debug!("No specified genesis in config. Using default genesis.");
            let bz = include_bytes!("mainnet/genesis.car");
            let reader = BufReader::<&[u8]>::new(bz.as_ref());
            process_car(reader, state_manager.chain_store()).await?
        }
    };

    info!("Initialized genesis: {}", genesis);

    // Get network name from genesis state.
    let network_name = state_manager
        .get_network_name(genesis.state_root())
        .map_err(|e| format!("Failed to retrieve network name from genesis: {}", e))?;
    Ok((Tipset::new(vec![genesis])?, network_name))
}

async fn process_car<R, BS>(
    reader: R,
    chain_store: &ChainStore<BS>,
) -> Result<BlockHeader, Box<dyn StdError>>
where
    R: AsyncRead + Send + Unpin,
    BS: BlockStore + Send + Sync + 'static,
{
    // Load genesis state into the database and get the Cid
    let genesis_cids: Vec<Cid> = load_car(chain_store.blockstore(), reader).await?;
    if genesis_cids.len() != 1 {
        panic!("Invalid Genesis. Genesis Tipset must have only 1 Block.");
    }

    let genesis_block: BlockHeader = chain_store.db.get(&genesis_cids[0])?.ok_or_else(|| {
        "Could not find genesis block despite being loaded using a genesis file".to_owned()
    })?;

    let store_genesis = chain_store.genesis()?;

    if store_genesis
        .map(|store| store == genesis_block)
        .unwrap_or_default()
    {
        debug!("Genesis from config matches Genesis from store");
        Ok(genesis_block)
    } else {
        debug!("Initialize ChainSyncer with new genesis from config");
        chain_store.set_genesis(&genesis_block)?;
        async_std::task::block_on(
            chain_store.set_heaviest_tipset(Arc::new(Tipset::new(vec![genesis_block.clone()])?)),
        )?;
        Ok(genesis_block)
    }
}

/// Import a chain from a CAR file. If the snapshot boolean is set, it will not verify the chain
/// state and instead accept the largest height as genesis.
pub async fn import_chain<V: ProofVerifier, DB>(
    sm: &Arc<StateManager<DB>>,
    path: &str,
    validate_height: Option<i64>,
) -> Result<(), Box<dyn std::error::Error>>
where
    DB: BlockStore + Send + Sync + 'static,
{
    let is_remote_file: bool = path.starts_with("http://") || path.starts_with("https://");

    info!("Importing chain from snapshot");
    // start import
    let cids = if is_remote_file {
        let url = Url::parse(path).expect("URL is invalid");
        info!("Downloading file...");
        let reader = FetchProgress::try_from(url)?;
        load_car(sm.blockstore(), reader).await?
    } else {
        let file = File::open(&path)
            .await
            .expect("Snapshot file path not found!");
        info!("Reading file...");
        let reader = FetchProgress::try_from(file)?;
        load_car(sm.blockstore(), reader).await?
    };
    let ts = sm
        .chain_store()
        .tipset_from_keys(&TipsetKeys::new(cids))
        .await?;
    let gb = sm
        .chain_store()
        .tipset_by_height(0, ts.clone(), true)
        .await?;

    if let Some(height) = validate_height {
        info!("Validating imported chain");
        sm.validate_chain::<V>(ts.clone(), height).await?;
    }
    let gen_cid = sm.chain_store().set_genesis(&gb.blocks()[0])?;
    sm.blockstore()
        .write(chain::HEAD_KEY, ts.key().marshal_cbor()?)?;
    info!(
        "Accepting {:?} as new head with genesis {:?}",
        ts.cids(),
        gen_cid
    );
    Ok(())
}
