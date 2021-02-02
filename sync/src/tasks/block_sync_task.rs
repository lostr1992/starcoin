// Copyright (c) The Starcoin Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::sync_metrics::SYNC_METRICS;
use crate::tasks::{BlockConnectedEvent, BlockConnectedEventHandle, BlockFetcher, BlockLocalStore};
use anyhow::{format_err, Result};
use futures::future::BoxFuture;
use futures::FutureExt;
use logger::prelude::*;
use network_api::NetworkService;
use starcoin_accumulator::{Accumulator, MerkleAccumulator};
use starcoin_chain::{verifier::BasicVerifier, BlockChain};
use starcoin_chain_api::{ChainReader, ChainWriter, ConnectBlockError, ExecutedBlock};
use starcoin_types::block::{Block, BlockInfo, BlockNumber};
use starcoin_types::peer_info::PeerId;
use starcoin_vm_types::on_chain_config::GlobalTimeOnChain;
use std::collections::HashMap;
use std::sync::Arc;
use stream_task::{CollectorState, TaskResultCollector, TaskState};

#[derive(Clone, Debug)]
pub struct SyncBlockData {
    pub(crate) block: Block,
    pub(crate) info: Option<BlockInfo>,
    pub(crate) peer_id: Option<PeerId>,
}

impl SyncBlockData {
    pub fn new(block: Block, block_info: Option<BlockInfo>, peer_id: Option<PeerId>) -> Self {
        Self {
            block,
            info: block_info,
            peer_id,
        }
    }
}

impl Into<(Block, Option<BlockInfo>, Option<PeerId>)> for SyncBlockData {
    fn into(self) -> (Block, Option<BlockInfo>, Option<PeerId>) {
        (self.block, self.info, self.peer_id)
    }
}

#[derive(Clone)]
pub struct BlockSyncTask {
    accumulator: Arc<MerkleAccumulator>,
    start_number: BlockNumber,
    fetcher: Arc<dyn BlockFetcher>,
    // if check_local_store is true, get block from local first.
    check_local_store: bool,
    local_store: Arc<dyn BlockLocalStore>,
    batch_size: u64,
}

impl BlockSyncTask {
    pub fn new<F, S>(
        accumulator: MerkleAccumulator,
        start_number: BlockNumber,
        fetcher: F,
        check_local_store: bool,
        local_store: S,
        batch_size: u64,
    ) -> Self
    where
        F: BlockFetcher + 'static,
        S: BlockLocalStore + 'static,
    {
        Self {
            accumulator: Arc::new(accumulator),
            start_number,
            fetcher: Arc::new(fetcher),
            check_local_store,
            local_store: Arc::new(local_store),
            batch_size,
        }
    }
}

impl TaskState for BlockSyncTask {
    type Item = SyncBlockData;

    fn new_sub_task(self) -> BoxFuture<'static, Result<Vec<Self::Item>>> {
        async move {
            let block_ids =
                self.accumulator
                    .get_leaves(self.start_number, false, self.batch_size)?;
            if block_ids.is_empty() {
                return Ok(vec![]);
            }
            if self.check_local_store {
                let block_with_info = self.local_store.get_block_with_info(block_ids.clone())?;
                let (no_exist_block_ids, result_map) =
                    block_ids.clone().into_iter().zip(block_with_info).fold(
                        (vec![], HashMap::new()),
                        |(mut no_exist_block_ids, mut result_map), (block_id, block_with_info)| {
                            match block_with_info {
                                Some(block_data) => {
                                    result_map.insert(block_id, block_data);
                                }
                                None => {
                                    no_exist_block_ids.push(block_id);
                                }
                            }
                            (no_exist_block_ids, result_map)
                        },
                    );
                debug!(
                    "[sync] get_block_with_info from local store, ids: {}, found: {}",
                    block_ids.len(),
                    result_map.len()
                );
                let mut result_map = if no_exist_block_ids.is_empty() {
                    result_map
                } else {
                    self.fetcher
                        .fetch_block(no_exist_block_ids)
                        .await?
                        .into_iter()
                        .fold(result_map, |mut result_map, (block, peer_id)| {
                            result_map.insert(block.id(), SyncBlockData::new(block, None, peer_id));
                            result_map
                        })
                };
                //ensure return block's order same as request block_id's order.
                let result: Result<Vec<SyncBlockData>> = block_ids
                    .iter()
                    .map(|block_id| {
                        result_map
                            .remove(block_id)
                            .ok_or_else(|| format_err!("Get block by id {:?} failed", block_id))
                    })
                    .collect();
                result
            } else {
                Ok(self
                    .fetcher
                    .fetch_block(block_ids)
                    .await?
                    .into_iter()
                    .map(|(block, peer_id)| SyncBlockData::new(block, None, peer_id))
                    .collect())
            }
        }
        .boxed()
    }

    fn next(&self) -> Option<Self> {
        let next_start_number = self.start_number + self.batch_size;
        if next_start_number > self.accumulator.num_leaves() {
            None
        } else {
            Some(Self {
                accumulator: self.accumulator.clone(),
                start_number: next_start_number,
                fetcher: self.fetcher.clone(),
                check_local_store: self.check_local_store,
                local_store: self.local_store.clone(),
                batch_size: self.batch_size,
            })
        }
    }

    fn total_items(&self) -> Option<u64> {
        Some(self.accumulator.num_leaves() - self.start_number)
    }
}

pub struct BlockCollector<N, H>
where
    N: NetworkService + 'static,
    H: BlockConnectedEventHandle + 'static,
{
    //node's current block info
    current_block_info: BlockInfo,
    // the block chain init by ancestor
    chain: BlockChain,
    event_handle: H,
    network: N,
    skip_pow_verify: bool,
}

impl<N, H> BlockCollector<N, H>
where
    N: NetworkService + 'static,
    H: BlockConnectedEventHandle + 'static,
{
    pub fn new_with_handle(
        current_block_info: BlockInfo,
        chain: BlockChain,
        event_handle: H,
        network: N,
        skip_pow_verify: bool,
    ) -> Self {
        Self {
            current_block_info,
            chain,
            event_handle,
            network,
            skip_pow_verify,
        }
    }

    #[cfg(test)]
    pub fn apply_block_for_test(&mut self, block: Block) -> Result<()> {
        self.apply_block(block, None)
    }

    fn apply_block(&mut self, block: Block, peer_id: Option<PeerId>) -> Result<()> {
        let _timer = SYNC_METRICS
            .sync_apply_block_time
            .with_label_values(&["time"])
            .start_timer();
        if let Err(err) = if self.skip_pow_verify {
            self.chain
                .apply_with_verifier::<BasicVerifier>(block.clone())
        } else {
            self.chain.apply(block.clone())
        } {
            error!(
                "[sync] collect block error: {:?}, peer_id:{:?} ",
                err, peer_id
            );
            match err.downcast::<ConnectBlockError>() {
                Ok(connect_error) => match connect_error {
                    ConnectBlockError::FutureBlock(block) => {
                        Err(ConnectBlockError::FutureBlock(block).into())
                    }
                    e => {
                        self.chain.get_storage().save_failed_block(
                            block.id(),
                            block,
                            peer_id.clone(),
                            format!("{:?}", e),
                        )?;
                        if let Some(peer) = peer_id {
                            self.network.report_peer(peer, (&e).into());
                        }

                        Err(e.into())
                    }
                },
                Err(e) => Err(e),
            }
        } else {
            Ok(())
        }
    }
}

impl<N, H> TaskResultCollector<SyncBlockData> for BlockCollector<N, H>
where
    N: NetworkService + 'static,
    H: BlockConnectedEventHandle + 'static,
{
    type Output = BlockChain;

    fn collect(&mut self, item: SyncBlockData) -> Result<CollectorState> {
        let (block, block_info, peer_id) = item.into();
        let block_id = block.id();
        let timestamp = block.header().timestamp();
        match block_info {
            Some(block_info) => {
                //If block_info exists, it means that this block was already executed and try connect in the previous sync, but the sync task was interrupted.
                //So, we just need to update chain and continue
                self.chain.connect(ExecutedBlock { block, block_info })?;
            }
            None => {
                self.apply_block(block.clone(), peer_id)?;
                self.chain
                    .time_service()
                    .adjust(GlobalTimeOnChain::new(timestamp));
                let total_difficulty = self.chain.get_total_difficulty()?;
                // only try connect block when sync chain total_difficulty > node's current chain.
                if total_difficulty > self.current_block_info.total_difficulty {
                    if let Err(e) = self.event_handle.handle(BlockConnectedEvent { block }) {
                        error!(
                            "Send BlockConnectedEvent error: {:?}, block_id: {}",
                            e, block_id
                        );
                    }
                }
            }
        }

        Ok(CollectorState::Need)
    }

    fn finish(self) -> Result<Self::Output> {
        Ok(self.chain)
    }
}
