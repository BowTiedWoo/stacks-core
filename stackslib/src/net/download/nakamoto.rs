// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::TryFrom;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};

use rand::seq::SliceRandom;
use rand::{thread_rng, RngCore};
use stacks_common::types::chainstate::{
    BlockHeaderHash, ConsensusHash, PoxId, SortitionId, StacksBlockId,
};
use stacks_common::types::net::{PeerAddress, PeerHost};
use stacks_common::types::StacksEpochId;
use stacks_common::util::hash::to_hex;
use stacks_common::util::secp256k1::{Secp256k1PrivateKey, Secp256k1PublicKey};
use stacks_common::util::{get_epoch_time_ms, get_epoch_time_secs, log};
use wsts::curve::point::Point;

use crate::burnchains::{Burnchain, BurnchainView, PoxConstants};
use crate::chainstate::burn::db::sortdb::{
    BlockHeaderCache, SortitionDB, SortitionDBConn, SortitionHandleConn,
};
use crate::chainstate::burn::BlockSnapshot;
use crate::chainstate::nakamoto::{
    NakamotoBlock, NakamotoBlockHeader, NakamotoChainState, NakamotoStagingBlocksConnRef,
};
use crate::chainstate::stacks::db::StacksChainState;
use crate::chainstate::stacks::{
    Error as chainstate_error, StacksBlockHeader, TenureChangePayload,
};
use crate::core::{
    EMPTY_MICROBLOCK_PARENT_HASH, FIRST_BURNCHAIN_CONSENSUS_HASH, FIRST_STACKS_BLOCK_HASH,
};
use crate::net::api::gettenureinfo::RPCGetTenureInfo;
use crate::net::chat::ConversationP2P;
use crate::net::db::{LocalPeer, PeerDB};
use crate::net::http::HttpRequestContents;
use crate::net::httpcore::{StacksHttpRequest, StacksHttpResponse};
use crate::net::inv::epoch2x::InvState;
use crate::net::inv::nakamoto::{NakamotoInvStateMachine, NakamotoTenureInv};
use crate::net::neighbors::rpc::NeighborRPC;
use crate::net::neighbors::NeighborComms;
use crate::net::p2p::PeerNetwork;
use crate::net::server::HttpPeer;
use crate::net::{Error as NetError, Neighbor, NeighborAddress, NeighborKey};
use crate::util_lib::db::{DBConn, Error as DBError};

/// Download states for an historic tenure
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NakamotoTenureDownloadState {
    /// Getting the tenure-start block
    GetTenureStartBlock(StacksBlockId),
    /// Waiting for the child tenure's tenure-start block to arrive, which is usually (but not
    /// always) handled by the execution of another NakamotoTenureDownloader.  The only
    /// exception is if this tenure contains the anchor block, and it's the last tenure in the
    /// reward cycle.  In this case, the end-block must be directly fetched, since there will be no
    /// follow-on NakamotTenureDownloader in the same reward cycle who can provide this.
    WaitForTenureEndBlock(StacksBlockId),
    /// Gettin the tenure-end block directly.  This only happens for tenures whose end-blocks
    /// cannot be provided by tenure downloaders within the same reward cycle.
    GetTenureEndBlock(StacksBlockId),
    /// Receiving tenure blocks
    GetTenureBlocks(StacksBlockId),
    /// We have gotten all the blocks for this tenure
    Done,
}

impl fmt::Display for NakamotoTenureDownloadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Download state machine for an historic tenure -- a tenure for which the start and end block IDs
/// can be inferred from the chainstate and a peer's inventory (this excludes the two most recent
/// tenures).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NakamotoTenureDownloader {
    /// Consensus hash that identifies this tenure
    pub tenure_id_consensus_hash: ConsensusHash,
    /// Stacks block ID of the tenure-start block.  Learend from the inventory state machine and
    /// sortition DB.
    pub tenure_start_block_id: StacksBlockId,
    /// Stacks block ID of the last block in this tenure (this will be the tenure-start block ID
    /// for some other tenure).  Learned from the inventory state machine and sortition DB.
    pub tenure_end_block_id: StacksBlockId,
    /// Address of who we're asking
    pub naddr: NeighborAddress,
    /// Aggregate public key that signed the start-block of this tenure
    pub start_aggregate_public_key: Point,
    /// Aggregate public key that signed the end-block of this tenure
    pub end_aggregate_public_key: Point,
    /// Whether or not we're idle -- i.e. the next request can begin
    pub idle: bool,

    /// What state we're in for downloading this tenure
    pub state: NakamotoTenureDownloadState,
    /// Tenure-start block
    pub tenure_start_block: Option<NakamotoBlock>,
    /// Pre-stored tenure end block (used by the unconfirmed block downloader)
    pub tenure_end_block: Option<NakamotoBlock>,
    /// Tenure-end block header and TenureChange
    pub tenure_end_header: Option<(NakamotoBlockHeader, TenureChangePayload)>,
    /// Tenure blocks
    pub tenure_blocks: Option<Vec<NakamotoBlock>>,
}

impl NakamotoTenureDownloader {
    pub fn new(
        tenure_id_consensus_hash: ConsensusHash,
        tenure_start_block_id: StacksBlockId,
        tenure_end_block_id: StacksBlockId,
        naddr: NeighborAddress,
        start_aggregate_public_key: Point,
        end_aggregate_public_key: Point,
    ) -> Self {
        test_debug!(
            "Instantiate downloader to {} for tenure {}",
            &naddr,
            &tenure_id_consensus_hash
        );
        Self {
            tenure_id_consensus_hash,
            tenure_start_block_id,
            tenure_end_block_id,
            naddr,
            start_aggregate_public_key,
            end_aggregate_public_key,
            idle: false,
            state: NakamotoTenureDownloadState::GetTenureStartBlock(tenure_start_block_id.clone()),
            tenure_start_block: None,
            tenure_end_header: None,
            tenure_end_block: None,
            tenure_blocks: None,
        }
    }

    pub fn with_tenure_end_block(mut self, tenure_end_block: NakamotoBlock) -> Self {
        self.tenure_end_block = Some(tenure_end_block);
        self
    }

    /// Is this downloader waiting for the tenure-end block data from some other downloader?
    pub fn is_waiting(&self) -> bool {
        if let NakamotoTenureDownloadState::WaitForTenureEndBlock(..) = self.state {
            return true;
        } else {
            return false;
        }
    }

    /// Create a tenure-downloader with a known start and end block.
    /// This runs the state-transitions for receiving these two blocks, so they'll be validated
    /// against the given aggregate public key.
    /// Returns Ok(downloader) on success
    /// Returns Err(..) if we fail to validate these blocks
    pub fn from_start_end_blocks(
        tenure_start_block: NakamotoBlock,
        tenure_end_block: NakamotoBlock,
        naddr: NeighborAddress,
        start_aggregate_public_key: Point,
        end_aggregate_public_key: Point,
    ) -> Result<Self, NetError> {
        let mut downloader = Self::new(
            tenure_start_block.header.consensus_hash.clone(),
            tenure_start_block.header.block_id(),
            tenure_end_block.header.block_id(),
            naddr,
            start_aggregate_public_key,
            end_aggregate_public_key,
        );
        downloader.try_accept_tenure_start_block(tenure_start_block)?;
        downloader.try_accept_tenure_end_block(&tenure_end_block)?;
        Ok(downloader)
    }

    /// Validate and accept a given tenure-start block.  If accepted, then advance the state.
    pub fn try_accept_tenure_start_block(
        &mut self,
        tenure_start_block: NakamotoBlock,
    ) -> Result<(), NetError> {
        let NakamotoTenureDownloadState::GetTenureStartBlock(_) = &self.state else {
            warn!("Invalid state for this method";
                  "state" => %self.state);
            return Err(NetError::InvalidState);
        };

        if self.tenure_start_block_id != tenure_start_block.header.block_id() {
            warn!("Invalid tenure-start block"; 
                  "tenure_id" => %self.tenure_id_consensus_hash,
                  "tenure_id_start_block" => %self.tenure_start_block_id,
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        let schnorr_signature = &tenure_start_block.header.signer_signature.0;
        let message = tenure_start_block.header.signer_signature_hash().0;
        if !schnorr_signature.verify(&self.start_aggregate_public_key, &message) {
            warn!("Invalid tenure-start block: bad signer signature";
                  "tenure_id" => %self.tenure_id_consensus_hash,
                  "block.header.block_id" => %tenure_start_block.header.block_id(),
                  "start_aggregate_public_key" => %self.start_aggregate_public_key,
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        test_debug!(
            "Accepted tenure-start block for tenure {} block={}",
            &self.tenure_id_consensus_hash,
            &tenure_start_block.block_id()
        );
        self.tenure_start_block = Some(tenure_start_block);

        if let Some((hdr, _tc_payload)) = self.tenure_end_header.as_ref() {
            // tenure_end_header supplied externally
            self.state = NakamotoTenureDownloadState::GetTenureBlocks(hdr.parent_block_id.clone());
        } else if let Some(tenure_end_block) = self.tenure_end_block.take() {
            test_debug!(
                "Preemptively process tenure-end block {} for tenure {}",
                tenure_end_block.block_id(),
                &self.tenure_id_consensus_hash
            );
            self.state =
                NakamotoTenureDownloadState::WaitForTenureEndBlock(tenure_end_block.block_id());
            self.try_accept_tenure_end_block(&tenure_end_block)?;
        } else {
            // need to get tenure_end_header.  By default, assume that another
            // NakamotoTenureDownlaoder will provide this block, and allow the
            // NakamotoTenureDownloaderSet instance that maanges a collection of these
            // state-machines make the call to require this one to fetch the block directly.
            self.state = NakamotoTenureDownloadState::WaitForTenureEndBlock(
                self.tenure_end_block_id.clone(),
            );
        }
        Ok(())
    }

    /// Transition this state-machine from waiting for its tenure-end block from another
    /// state-machine to directly fetching it.  This only needs to happen if the tenure this state
    /// machine is downloading contains the PoX anchor block, and it's also the last confirmed
    /// tenurein this reward cycle.
    pub fn transition_to_fetch_end_block(&mut self) -> Result<(), NetError> {
        let NakamotoTenureDownloadState::WaitForTenureEndBlock(end_block_id) = self.state else {
            return Err(NetError::InvalidState);
        };
        self.state = NakamotoTenureDownloadState::GetTenureEndBlock(end_block_id);
        Ok(())
    }

    /// Validate and accept a tenure-end block.  If accepted, then advance the state.
    pub fn try_accept_tenure_end_block(
        &mut self,
        tenure_end_block: &NakamotoBlock,
    ) -> Result<(), NetError> {
        if !matches!(
            &self.state,
            NakamotoTenureDownloadState::WaitForTenureEndBlock(_)
        ) && !matches!(
            &self.state,
            NakamotoTenureDownloadState::GetTenureEndBlock(_)
        ) {
            warn!("Invalid state for this method";
                  "state" => %self.state);
            return Err(NetError::InvalidState);
        };
        let Some(tenure_start_block) = self.tenure_start_block.as_ref() else {
            return Err(NetError::InvalidState);
        };

        // must be expected
        if self.tenure_end_block_id != tenure_end_block.header.block_id() {
            warn!("Invalid tenure-end block";
                  "tenure_id" => %self.tenure_id_consensus_hash,
                  "tenure_id_end_block" => %self.tenure_end_block_id,
                  "block.header.block_id" => %tenure_end_block.header.block_id(),
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        let schnorr_signature = &tenure_end_block.header.signer_signature.0;
        let message = tenure_end_block.header.signer_signature_hash().0;
        if !schnorr_signature.verify(&self.end_aggregate_public_key, &message) {
            warn!("Invalid tenure-end block: bad signer signature";
                  "tenure_id" => %self.tenure_id_consensus_hash,
                  "block.header.block_id" => %tenure_end_block.header.block_id(),
                  "end_aggregate_public_key" => %self.end_aggregate_public_key,
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        // extract the needful -- need the tenure-change payload (which proves that the tenure-end
        // block is the tenure-start block for the next tenure) and the parent block ID (which is
        // the next block to download).
        let Ok(valid) = tenure_end_block.is_wellformed_tenure_start_block() else {
            warn!("Invalid tenure-end block: failed to validate tenure-start";
                  "block_id" => %tenure_end_block.block_id());
            return Err(NetError::InvalidMessage);
        };

        if !valid {
            warn!("Invalid tenure-end block: not a well-formed tenure-start block";
                  "block_id" => %tenure_end_block.block_id());
            return Err(NetError::InvalidMessage);
        }

        let Some(tc_payload) = tenure_end_block.try_get_tenure_change_payload() else {
            warn!("Invalid tenure-end block: no tenure-change transaction";
                  "block_id" => %tenure_end_block.block_id());
            return Err(NetError::InvalidMessage);
        };

        // tc_payload must point to the tenure-start block's header
        if tc_payload.prev_tenure_consensus_hash != tenure_start_block.header.consensus_hash {
            warn!("Invalid tenure-end block: tenure-change does not point to tenure-start block";
                  "start_block_id" => %tenure_start_block.block_id(),
                  "end_block_id" => %tenure_end_block.block_id(),
                  "tc_payload.prev_tenure_consensus_hash" => %tc_payload.prev_tenure_consensus_hash,
                  "tenure_start.consensus_hash" => %tenure_start_block.header.consensus_hash);
            return Err(NetError::InvalidMessage);
        }

        test_debug!(
            "Accepted tenure-end header for tenure {} block={}; expect {} blocks",
            &self.tenure_id_consensus_hash,
            &tenure_end_block.block_id(),
            tc_payload.previous_tenure_blocks
        );
        self.tenure_end_header = Some((tenure_end_block.header.clone(), tc_payload.clone()));
        self.state = NakamotoTenureDownloadState::GetTenureBlocks(
            tenure_end_block.header.parent_block_id.clone(),
        );
        Ok(())
    }

    /// Determine how many blocks must be in this tenure.
    /// Returns None if we don't have the start and end blocks yet.
    pub fn tenure_length(&self) -> Option<u64> {
        self.tenure_end_header
            .as_ref()
            .map(|(_hdr, tc_payload)| u64::from(tc_payload.previous_tenure_blocks))
    }

    /// Add downloaded tenure blocks.
    /// If we have collected all tenure blocks, then return them.
    pub fn try_accept_tenure_blocks(
        &mut self,
        mut tenure_blocks: Vec<NakamotoBlock>,
    ) -> Result<Option<Vec<NakamotoBlock>>, NetError> {
        let NakamotoTenureDownloadState::GetTenureBlocks(block_cursor) = &self.state else {
            warn!("Invalid state for this method";
                  "state" => %self.state);
            return Err(NetError::InvalidState);
        };

        if tenure_blocks.is_empty() {
            // nothing to do
            return Ok(None);
        }

        // blocks must be contiguous and in order from highest to lowest
        let mut expected_block_id = block_cursor;
        let mut count = 0;
        for block in tenure_blocks.iter() {
            if &block.header.block_id() != expected_block_id {
                warn!("Unexpected Nakamoto block -- not part of tenure";
                      "expected_block_id" => %expected_block_id,
                      "block_id" => %block.header.block_id(),
                      "state" => %self.state);
                return Err(NetError::InvalidMessage);
            }

            let schnorr_signature = &block.header.signer_signature.0;
            let message = block.header.signer_signature_hash().0;
            if !schnorr_signature.verify(&self.start_aggregate_public_key, &message) {
                warn!("Invalid block: bad signer signature";
                      "tenure_id" => %self.tenure_id_consensus_hash,
                      "block.header.block_id" => %block.header.block_id(),
                      "start_aggregate_public_key" => %self.start_aggregate_public_key,
                      "state" => %self.state);
                return Err(NetError::InvalidMessage);
            }

            expected_block_id = &block.header.parent_block_id;
            count += 1;
            if self
                .tenure_blocks
                .as_ref()
                .map(|blocks| blocks.len())
                .unwrap_or(0)
                .saturating_add(count)
                > self.tenure_length().unwrap_or(0) as usize
            {
                warn!("Invalid blocks: exceeded {} tenure blocks", self.tenure_length().unwrap_or(0);
                      "tenure_id" => %self.tenure_id_consensus_hash,
                      "count" => %count,
                      "tenure_length" => self.tenure_length().unwrap_or(0),
                      "num_blocks" => tenure_blocks.len());
                return Err(NetError::InvalidMessage);
            }
        }

        if let Some(blocks) = self.tenure_blocks.as_mut() {
            blocks.append(&mut tenure_blocks);
        } else {
            self.tenure_blocks = Some(tenure_blocks);
        }

        // did we reach the tenure start block?
        let Some(blocks) = self.tenure_blocks.as_ref() else {
            // unreachable but be defensive
            warn!("Invalid state: no blocks (infallible -- got None)");
            return Err(NetError::InvalidState);
        };

        let Some(earliest_block) = blocks.last() else {
            // unreachable but be defensive
            warn!("Invalid state: no blocks (infallible -- got empty vec)");
            return Err(NetError::InvalidState);
        };

        let Some(tenure_start_block) = self.tenure_start_block.as_ref() else {
            // unreachable but be defensive
            warn!("Invalid state: no tenure-start block (infallible)");
            return Err(NetError::InvalidState);
        };

        test_debug!(
            "Accepted tenure blocks for tenure {} cursor={} ({})",
            &self.tenure_id_consensus_hash,
            &block_cursor,
            count
        );
        if earliest_block.block_id() != tenure_start_block.block_id() {
            // still have more blocks to download
            let next_block_id = earliest_block.header.parent_block_id.clone();
            debug!(
                "Need more blocks for tenure {} (went from {} to {}, next is {})",
                &self.tenure_id_consensus_hash,
                &block_cursor,
                &earliest_block.block_id(),
                &next_block_id
            );
            self.state = NakamotoTenureDownloadState::GetTenureBlocks(next_block_id);
            return Ok(None);
        }

        // finished!
        self.state = NakamotoTenureDownloadState::Done;
        Ok(self
            .tenure_blocks
            .take()
            .map(|blocks| blocks.into_iter().rev().collect()))
    }

    /// Produce the next HTTP request that, when successfully executed, will advance this state
    /// machine.
    /// Not all states require an HTTP request for advanceement.
    ///
    /// Returns Ok(Some(request)) if a request is needed
    /// Returns Ok(None) if a request is not needed (i.e. we're waiting for some other machine's
    /// state)
    /// Returns Err(..) if we're done.
    pub fn make_next_download_request(
        &self,
        peerhost: PeerHost,
    ) -> Result<Option<StacksHttpRequest>, ()> {
        let request = match self.state {
            NakamotoTenureDownloadState::GetTenureStartBlock(start_block_id) => {
                test_debug!("Request tenure-start block {}", &start_block_id);
                StacksHttpRequest::new_get_nakamoto_block(peerhost, start_block_id.clone())
            }
            NakamotoTenureDownloadState::WaitForTenureEndBlock(_block_id) => {
                // we're waiting for some other downloader's block-fetch to complete
                test_debug!("Waiting for tenure-end block {}", &_block_id);
                return Ok(None);
            }
            NakamotoTenureDownloadState::GetTenureEndBlock(end_block_id) => {
                test_debug!("Request tenure-end block {}", &end_block_id);
                StacksHttpRequest::new_get_nakamoto_block(peerhost, end_block_id.clone())
            }
            NakamotoTenureDownloadState::GetTenureBlocks(end_block_id) => {
                test_debug!("Downloading tenure ending at {}", &end_block_id);
                StacksHttpRequest::new_get_nakamoto_tenure(peerhost, end_block_id.clone(), None)
            }
            NakamotoTenureDownloadState::Done => {
                // nothing more to do
                return Err(());
            }
        };
        Ok(Some(request))
    }

    /// Begin the next download request for this state machine.
    /// Returns Ok(Some(true)) if we sent the request, or there's already an in-flight request
    /// Returns Ok(Some(false)) if not (e.g. neighbor is known to be dead or broken)
    /// Returns Ok(None) if this state machine is blocked
    pub fn send_next_download_request(
        &mut self,
        network: &mut PeerNetwork,
        neighbor_rpc: &mut NeighborRPC,
    ) -> Result<Option<bool>, NetError> {
        if neighbor_rpc.has_inflight(&self.naddr) {
            test_debug!("Peer {} has an inflight request", &self.naddr);
            return Ok(Some(true));
        }
        if neighbor_rpc.is_dead_or_broken(network, &self.naddr) {
            return Err(NetError::PeerNotConnected);
        }

        let Some(peerhost) = NeighborRPC::get_peer_host(network, &self.naddr) else {
            // no conversation open to this neighbor
            neighbor_rpc.add_dead(network, &self.naddr);
            return Err(NetError::PeerNotConnected);
        };

        let request = match self.make_next_download_request(peerhost) {
            Ok(Some(request)) => request,
            Ok(None) => {
                return Ok(Some(true));
            }
            Err(_) => {
                return Ok(Some(false));
            }
        };

        neighbor_rpc.send_request(network, self.naddr.clone(), request)?;
        self.idle = false;
        Ok(Some(true))
    }

    /// Handle a received StacksHttpResponse.
    /// If we get the full tenure, return it.
    pub fn handle_next_download_response(
        &mut self,
        response: StacksHttpResponse,
    ) -> Result<Option<Vec<NakamotoBlock>>, NetError> {
        self.idle = true;
        match self.state {
            NakamotoTenureDownloadState::GetTenureStartBlock(_block_id) => {
                test_debug!(
                    "Got download response for tenure-start block {}",
                    &_block_id
                );
                let block = response.decode_nakamoto_block()?;
                self.try_accept_tenure_start_block(block)?;
                Ok(None)
            }
            NakamotoTenureDownloadState::WaitForTenureEndBlock(..) => {
                test_debug!("Invalid state -- Got download response for WaitForTenureBlock");
                Err(NetError::InvalidState)
            }
            NakamotoTenureDownloadState::GetTenureEndBlock(_block_id) => {
                test_debug!("Got download response to tenure-end block {}", &_block_id);
                let block = response.decode_nakamoto_block()?;
                self.try_accept_tenure_end_block(&block)?;
                Ok(None)
            }
            NakamotoTenureDownloadState::GetTenureBlocks(_end_block_id) => {
                test_debug!(
                    "Got download response for tenure blocks ending at {}",
                    &_end_block_id
                );
                let blocks = response.decode_nakamoto_tenure()?;
                self.try_accept_tenure_blocks(blocks)
            }
            NakamotoTenureDownloadState::Done => Err(NetError::InvalidState),
        }
    }

    pub fn is_done(&self) -> bool {
        self.state == NakamotoTenureDownloadState::Done
    }
}

/// Download states for a unconfirmed tenures
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NakamotoUnconfirmedDownloadState {
    /// Getting the tenure tip information
    GetTenureInfo,
    /// Get the tenure start block for the ongoing tenure
    GetTenureStartBlock(StacksBlockId),
    /// Receiving unconfirmed tenure blocks
    GetUnconfirmedTenureBlocks(StacksBlockId),
    /// We have gotten all the unconfirmed blocks for this tenure, and we now have the end block
    /// for the highest complete tenure (which can now be obtained via `NakamotoTenureDownloadState`).
    Done,
}

impl fmt::Display for NakamotoUnconfirmedDownloadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Download state machine for the unconfirmed tenures
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NakamotoUnconfirmedTenureDownloader {
    /// state of this machine
    pub state: NakamotoUnconfirmedDownloadState,
    /// Address of who we're asking
    pub naddr: NeighborAddress,
    /// Aggregate public key of the highest confirmed tenure
    pub confirmed_aggregate_public_key: Option<Point>,
    /// Aggregate public key of the unconfirmed (ongoing) tenure
    pub unconfirmed_aggregate_public_key: Option<Point>,
    /// Block ID of this node's highest-processed block
    pub highest_processed_block_id: Option<StacksBlockId>,
    /// Highest processed block height  (which may not need to be loaded)
    pub highest_processed_block_height: Option<u64>,

    /// Tenure tip info we obtained for this peer
    pub tenure_tip: Option<RPCGetTenureInfo>,
    /// Tenure start block for the ongoing tip.
    /// This is also the tenure-end block for the highest-complete tip.
    pub unconfirmed_tenure_start_block: Option<NakamotoBlock>,
    /// Unconfirmed tenure blocks obtained
    pub unconfirmed_tenure_blocks: Option<Vec<NakamotoBlock>>,
}

impl NakamotoUnconfirmedTenureDownloader {
    pub fn new(naddr: NeighborAddress, highest_processed_block_id: Option<StacksBlockId>) -> Self {
        Self {
            state: NakamotoUnconfirmedDownloadState::GetTenureInfo,
            naddr,
            confirmed_aggregate_public_key: None,
            unconfirmed_aggregate_public_key: None,
            highest_processed_block_id,
            highest_processed_block_height: None,
            tenure_tip: None,
            unconfirmed_tenure_start_block: None,
            unconfirmed_tenure_blocks: None,
        }
    }

    pub fn unconfirmed_tenure_id(&self) -> Option<&ConsensusHash> {
        self.tenure_tip.as_ref().map(|tt| &tt.consensus_hash)
    }

    /// Set the highest-processed block.
    /// This can be performed by the downloader itself in order to inform ongoing requests for
    /// unconfirmed tenures of newly-processed blocks, so they don't re-download blocks this node
    /// has already handled.
    pub fn set_highest_processed_block(
        &mut self,
        highest_processed_block_id: StacksBlockId,
        highest_processed_block_height: u64,
    ) {
        self.highest_processed_block_id = Some(highest_processed_block_id);
        self.highest_processed_block_height = Some(highest_processed_block_height);
    }

    /// Try and accept the tenure info.  It will be validated against the sortition DB and its tip.
    /// Remember:
    /// * tenure_tip.consensus_hash
    ///     This is the consensus hash of the remote node's ongoing tenure. It may not be the
    ///     sortition tip.
    /// * tenure_tip.tenure_start_block_id
    ///     This is the first block ID of the ongoing unconfirmed tenure.
    /// * tenure_tip.parent_consensus_hash
    ///     This is the consensus hash of the parent of the ongoing tenure. It's the node's highest
    ///     complete tenure, for which we know the start and end block IDs.
    /// * tenure_tip.parent_tenure_start_block_id
    ///     This is the tenure start block for the highest complete tenure.  It should be equal to
    ///     the winning Stacks block hash of the snapshot for the ongoing tenure.
    ///  
    /// We may already have the tenure-start block for the unconfirmed tenure. If so, then don't go
    /// fetch it again; just get the new unconfirmed blocks.
    pub fn try_accept_tenure_info(
        &mut self,
        sortdb: &SortitionDB,
        sort_tip: &BlockSnapshot,
        chainstate: &StacksChainState,
        tenure_tip: RPCGetTenureInfo,
        agg_pubkeys: &BTreeMap<u64, Option<Point>>,
    ) -> Result<(), NetError> {
        if self.state != NakamotoUnconfirmedDownloadState::GetTenureInfo {
            return Err(NetError::InvalidState);
        }
        if self.tenure_tip.is_some() {
            return Err(NetError::InvalidState);
        }

        // authenticate consensus hashes against canonical chain history
        let tenure_sn =
            SortitionDB::get_block_snapshot_consensus(sortdb.conn(), &tenure_tip.consensus_hash)?
                .ok_or(NetError::DBError(DBError::NotFoundError))?;
        let parent_tenure_sn = SortitionDB::get_block_snapshot_consensus(
            sortdb.conn(),
            &tenure_tip.parent_consensus_hash,
        )?
        .ok_or(NetError::DBError(DBError::NotFoundError))?;

        let ih = sortdb.index_handle(&sort_tip.sortition_id);
        let ancestor_tenure_sn = ih
            .get_block_snapshot_by_height(tenure_sn.block_height)?
            .ok_or(NetError::DBError(DBError::NotFoundError))?;

        if ancestor_tenure_sn.sortition_id != tenure_sn.sortition_id {
            // .consensus_hash is not on the canonical fork
            warn!("Unconfirmed tenure consensus hash is not canonical";
                  "peer" => %self.naddr,
                  "consensus_hash" => %tenure_tip.consensus_hash);
            return Err(DBError::NotFoundError.into());
        }
        let ancestor_parent_tenure_sn = ih
            .get_block_snapshot_by_height(parent_tenure_sn.block_height)?
            .ok_or(NetError::DBError(DBError::NotFoundError.into()))?;

        if ancestor_parent_tenure_sn.sortition_id != parent_tenure_sn.sortition_id {
            // .parent_consensus_hash is not on the canonical fork
            warn!("Parent unconfirmed tenure consensus hash is not canonical";
                  "peer" => %self.naddr,
                  "consensus_hash" => %tenure_tip.parent_consensus_hash);
            return Err(DBError::NotFoundError.into());
        }

        // parent tenure sortition must precede the ongoing tenure sortition
        if tenure_sn.block_height <= parent_tenure_sn.block_height {
            warn!("Parent tenure snapshot is not an ancestor of the current tenure snapshot";
                  "peer" => %self.naddr,
                  "consensus_hash" => %tenure_tip.consensus_hash,
                  "parent_consensus_hash" => %tenure_tip.parent_consensus_hash);
            return Err(NetError::InvalidMessage);
        }

        // parent tenure start block ID must be the winning block hash for the ongoing tenure's
        // snapshot
        if tenure_sn.winning_stacks_block_hash.0 != tenure_tip.parent_tenure_start_block_id.0 {
            warn!("Ongoing tenure does not commit to highest complete tenure's start block";
                  "tenure_tip.tenure_start_block_id" => %tenure_tip.tenure_start_block_id,
                  "tenure_sn.winning_stacks_block_hash" => %tenure_sn.winning_stacks_block_hash);
            return Err(NetError::InvalidMessage);
        }

        if let Some(highest_processed_block_id) = self.highest_processed_block_id.as_ref() {
            let highest_processed_block = chainstate
                .nakamoto_blocks_db()
                .get_nakamoto_block(highest_processed_block_id)?
                .ok_or(NetError::DBError(DBError::NotFoundError))?
                .0;

            let highest_processed_block_height = highest_processed_block.header.chain_length;
            self.highest_processed_block_height = Some(highest_processed_block_height);

            if &tenure_tip.tip_block_id == highest_processed_block_id
                || highest_processed_block_height > tenure_tip.tip_height
            {
                // nothing to do -- we're at or ahead of the remote peer, so finish up.
                // If we don't have the tenure-start block for the confirmed tenure that the remote
                // peer claims to have, then the remote peer has sent us invalid data and we should
                // treat it as such.
                let unconfirmed_tenure_start_block = chainstate
                    .nakamoto_blocks_db()
                    .get_nakamoto_block(&tenure_tip.tenure_start_block_id)?
                    .ok_or(NetError::InvalidMessage)?
                    .0;
                self.unconfirmed_tenure_start_block = Some(unconfirmed_tenure_start_block);
                self.state = NakamotoUnconfirmedDownloadState::Done;
            }
        }

        if self.state != NakamotoUnconfirmedDownloadState::Done {
            let tenure_rc = sortdb
                .pox_constants
                .block_height_to_reward_cycle(sortdb.first_block_height, tenure_sn.block_height)
                .expect("FATAL: sortition from before system start");
            let parent_tenure_rc = sortdb
                .pox_constants
                .block_height_to_reward_cycle(
                    sortdb.first_block_height,
                    parent_tenure_sn.block_height,
                )
                .expect("FATAL: sortition from before system start");

            let Some(Some(confirmed_aggregate_public_key)) =
                agg_pubkeys.get(&parent_tenure_rc).cloned()
            else {
                warn!(
                    "No aggregate public key for confirmed tenure {} (rc {})",
                    &parent_tenure_sn.consensus_hash, parent_tenure_rc
                );
                return Err(NetError::InvalidState);
            };

            let Some(Some(unconfirmed_aggregate_public_key)) = agg_pubkeys.get(&tenure_rc).cloned()
            else {
                warn!(
                    "No aggregate public key for confirmed tenure {} (rc {})",
                    &tenure_sn.consensus_hash, tenure_rc
                );
                return Err(NetError::InvalidState);
            };

            if chainstate
                .nakamoto_blocks_db()
                .has_nakamoto_block(&tenure_tip.tenure_start_block_id.clone())?
            {
                // proceed to get unconfirmed blocks
                let unconfirmed_tenure_start_block = chainstate
                    .nakamoto_blocks_db()
                    .get_nakamoto_block(&tenure_tip.tenure_start_block_id)?
                    .ok_or(NetError::DBError(DBError::NotFoundError))?
                    .0;
                self.unconfirmed_tenure_start_block = Some(unconfirmed_tenure_start_block);
                self.state = NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(
                    tenure_tip.tip_block_id.clone(),
                );
            } else {
                // get the tenure-start block first
                self.state = NakamotoUnconfirmedDownloadState::GetTenureStartBlock(
                    tenure_tip.tenure_start_block_id.clone(),
                );
            }

            test_debug!(
                "Will validate unconfirmed blocks with ({},{}) and ({},{})",
                &confirmed_aggregate_public_key,
                parent_tenure_rc,
                &unconfirmed_aggregate_public_key,
                tenure_rc
            );
            self.confirmed_aggregate_public_key = Some(confirmed_aggregate_public_key);
            self.unconfirmed_aggregate_public_key = Some(unconfirmed_aggregate_public_key);
        }
        self.tenure_tip = Some(tenure_tip);
        Ok(())
    }

    /// Validate and accept the unconfirmed tenure-start block.  If accepted, then advance the state.
    pub fn try_accept_unconfirmed_tenure_start_block(
        &mut self,
        unconfirmed_tenure_start_block: NakamotoBlock,
    ) -> Result<(), NetError> {
        let NakamotoUnconfirmedDownloadState::GetTenureStartBlock(tenure_start_block_id) =
            &self.state
        else {
            warn!("Invalid state for this method";
                  "state" => %self.state);
            return Err(NetError::InvalidState);
        };
        let Some(tenure_tip) = self.tenure_tip.as_ref() else {
            return Err(NetError::InvalidState);
        };
        let Some(unconfirmed_aggregate_public_key) = self.unconfirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        // stacker signature has to match the current aggregate public key
        let schnorr_signature = &unconfirmed_tenure_start_block.header.signer_signature.0;
        let message = unconfirmed_tenure_start_block
            .header
            .signer_signature_hash()
            .0;
        if !schnorr_signature.verify(unconfirmed_aggregate_public_key, &message) {
            warn!("Invalid tenure-start block: bad signer signature";
                  "tenure_start_block.header.consensus_hash" => %unconfirmed_tenure_start_block.header.consensus_hash,
                  "tenure_start_block.header.block_id" => %unconfirmed_tenure_start_block.header.block_id(),
                  "unconfirmed_aggregate_public_key" => %unconfirmed_aggregate_public_key,
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        // block has to match the expected hash
        if tenure_start_block_id != &unconfirmed_tenure_start_block.header.block_id() {
            warn!("Invalid tenure-start block"; 
                  "tenure_start_block.header.consensus_hash" => %unconfirmed_tenure_start_block.header.consensus_hash,
                  "tenure_id_start_block" => %tenure_start_block_id,
                  "state" => %self.state);
            return Err(NetError::InvalidMessage);
        }

        // furthermore, the block has to match the expected tenure ID
        if unconfirmed_tenure_start_block.header.consensus_hash != tenure_tip.consensus_hash {
            warn!("Invalid tenure-start block or tenure-tip: consensus hash mismatch";
                  "tenure_start_block.header.consensus_hash" => %unconfirmed_tenure_start_block.header.consensus_hash,
                  "tenure_tip.consensus_hash" => %tenure_tip.consensus_hash);
            return Err(NetError::InvalidMessage);
        }

        self.unconfirmed_tenure_start_block = Some(unconfirmed_tenure_start_block);
        self.state = NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(
            tenure_tip.tip_block_id.clone(),
        );
        Ok(())
    }

    /// Add downloaded unconfirmed tenure blocks.
    /// If we have collected all tenure blocks, then return them.
    /// Returns Ok(Some(list-of-blocks)) on success, in which case, `list-of-blocks` is the
    /// height-ordered sequence of blocks in this tenure.
    /// Returns Ok(None) if there are still blocks to fetch
    /// Returns Err(..) on invalid state or invalid block.
    pub fn try_accept_unconfirmed_tenure_blocks(
        &mut self,
        mut tenure_blocks: Vec<NakamotoBlock>,
    ) -> Result<Option<Vec<NakamotoBlock>>, NetError> {
        let NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(last_block_id) =
            &self.state
        else {
            return Err(NetError::InvalidState);
        };

        let Some(tenure_tip) = self.tenure_tip.as_ref() else {
            return Err(NetError::InvalidState);
        };
        let Some(unconfirmed_aggregate_public_key) = self.unconfirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        if tenure_blocks.is_empty() {
            // nothing to do
            return Ok(None);
        }

        // blocks must be contiguous and in order from highest to lowest.
        // If there's a tenure-start block, it must be last.
        let mut expected_block_id = last_block_id;
        let mut at_tenure_start = false;
        for (cnt, block) in tenure_blocks.iter().enumerate() {
            if &block.header.block_id() != expected_block_id {
                warn!("Unexpected Nakamoto block -- not part of tenure";
                      "expected_block_id" => %expected_block_id,
                      "block_id" => %block.header.block_id());
                return Err(NetError::InvalidMessage);
            }
            let schnorr_signature = &block.header.signer_signature.0;
            let message = block.header.signer_signature_hash().0;
            if !schnorr_signature.verify(unconfirmed_aggregate_public_key, &message) {
                warn!("Invalid block: bad signer signature";
                      "tenure_id" => %tenure_tip.consensus_hash,
                      "block.header.block_id" => %block.header.block_id(),
                      "unconfirmed_aggregate_public_key" => %unconfirmed_aggregate_public_key,
                      "state" => %self.state);
                return Err(NetError::InvalidMessage);
            }

            // we may or may not need the tenure-start block for the unconfirmed tenure.  But if we
            // do, make sure it's valid, and it's the last block we receive.
            let Ok(valid) = block.is_wellformed_tenure_start_block() else {
                warn!("Invalid tenure-start block";
                      "tenure_id" => %tenure_tip.consensus_hash,
                      "block.header.block_id" => %block.header.block_id(),
                      "state" => %self.state);
                return Err(NetError::InvalidMessage);
            };
            if valid {
                if block.header.block_id() != tenure_tip.tenure_start_block_id {
                    warn!("Unexpected tenure-start block";
                          "tenure_id" => %tenure_tip.consensus_hash,
                          "block.header.block_id" => %block.header.block_id(),
                          "tenure_tip.tenure_start_block_id" => %tenure_tip.tenure_start_block_id);
                    return Err(NetError::InvalidMessage);
                }

                if cnt.saturating_add(1) != tenure_blocks.len() {
                    warn!("Invalid tenure stream -- got tenure-start before end of tenure";
                          "tenure_id" => %tenure_tip.consensus_hash,
                          "block.header.block_id" => %block.header.block_id(),
                          "cnt" => cnt,
                          "len" => tenure_blocks.len(),
                          "state" => %self.state);
                    return Err(NetError::InvalidMessage);
                }

                at_tenure_start = true;
                break;
            }

            // NOTE: this field can get updated by the downloader while this state-machine is in
            // this state.
            if let Some(highest_processed_block_id) = self.highest_processed_block_id.as_ref() {
                if expected_block_id == highest_processed_block_id {
                    // got all the blocks we asked for
                    at_tenure_start = true;
                    break;
                }
            }

            // NOTE: this field can get updated by the downloader while this state-machine is in
            // this state.
            if let Some(highest_processed_block_height) =
                self.highest_processed_block_height.as_ref()
            {
                if &block.header.chain_length < highest_processed_block_height {
                    // no need to continue this download
                    debug!("Cancelling unconfirmed tenure download to {}: have processed block at height {} already", &self.naddr, highest_processed_block_height);
                    at_tenure_start = true;
                    break;
                }
            }

            expected_block_id = &block.header.parent_block_id;
        }

        if let Some(blocks) = self.unconfirmed_tenure_blocks.as_mut() {
            blocks.append(&mut tenure_blocks);
        } else {
            self.unconfirmed_tenure_blocks = Some(tenure_blocks);
        }

        if at_tenure_start {
            // we have all of the unconfirmed tenure blocks that were requested.
            // only return those newer than the highest block
            self.state = NakamotoUnconfirmedDownloadState::Done;
            let highest_processed_block_height =
                *self.highest_processed_block_height.as_ref().unwrap_or(&0);
            return Ok(self.unconfirmed_tenure_blocks.take().map(|blocks| {
                blocks
                    .into_iter()
                    .filter(|block| block.header.chain_length > highest_processed_block_height)
                    .rev()
                    .collect()
            }));
        }

        let Some(blocks) = self.unconfirmed_tenure_blocks.as_ref() else {
            // unreachable but be defensive
            warn!("Invalid state: no blocks (infallible -- got empty vec)");
            return Err(NetError::InvalidState);
        };

        // still have more to get
        let Some(earliest_block) = blocks.last() else {
            // unreachable but be defensive
            warn!("Invalid state: no blocks (infallible -- got empty vec)");
            return Err(NetError::InvalidState);
        };
        let next_block_id = earliest_block.header.parent_block_id.clone();

        self.state = NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(next_block_id);
        Ok(None)
    }

    /// Check to sese if we need to get the highest-complete tenure.
    pub fn need_highest_complete_tenure(
        &self,
        chainstate: &StacksChainState,
    ) -> Result<bool, NetError> {
        if self.state != NakamotoUnconfirmedDownloadState::Done {
            return Err(NetError::InvalidState);
        }
        let Some(unconfirmed_tenure_start_block) = self.unconfirmed_tenure_start_block.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        // if we've processed the unconfirmed tenure-start block already, then we've necessarily
        // downloaded and processed the highest-complete tenure already.
        Ok(!NakamotoChainState::has_block_header(
            chainstate.db(),
            &unconfirmed_tenure_start_block.header.block_id(),
            false,
        )?)
    }

    /// Create a NakamotoTenureDownloader for the highest complete tenure
    /// Its tenure-start block will need to get fetched.
    pub fn make_highest_complete_tenure_downloader(
        &self,
        highest_tenure: &WantedTenure,
        unconfirmed_tenure: &WantedTenure,
    ) -> Result<NakamotoTenureDownloader, NetError> {
        if self.state != NakamotoUnconfirmedDownloadState::Done {
            return Err(NetError::InvalidState);
        }
        let Some(unconfirmed_tenure_start_block) = self.unconfirmed_tenure_start_block.as_ref()
        else {
            return Err(NetError::InvalidState);
        };
        let Some(confirmed_aggregate_public_key) = self.confirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };
        let Some(unconfirmed_aggregate_public_key) = self.unconfirmed_aggregate_public_key.as_ref()
        else {
            return Err(NetError::InvalidState);
        };

        test_debug!(
            "Create highest complete tenure downloader for {}",
            &highest_tenure.tenure_id_consensus_hash
        );
        let ntd = NakamotoTenureDownloader::new(
            highest_tenure.tenure_id_consensus_hash.clone(),
            unconfirmed_tenure.winning_block_id.clone(),
            unconfirmed_tenure_start_block.header.block_id(),
            self.naddr.clone(),
            confirmed_aggregate_public_key.clone(),
            unconfirmed_aggregate_public_key.clone(),
        )
        .with_tenure_end_block(unconfirmed_tenure_start_block.clone());

        Ok(ntd)
    }

    /// Produce the next HTTP request that, when successfully executed, will advance this state
    /// machine.
    ///
    /// Returns Ok(Some(request)) if a request is needed
    /// Returns Ok(None) if a request is not needed -- i.e. we've gotten all of the information we
    /// can get, so go and get the highest full tenure.
    /// Returns Err(..) if we're done.
    pub fn make_next_download_request(
        &self,
        peerhost: PeerHost,
    ) -> Result<Option<StacksHttpRequest>, ()> {
        match &self.state {
            NakamotoUnconfirmedDownloadState::GetTenureInfo => {
                // need to get the tenure tip
                return Ok(Some(StacksHttpRequest::new_get_nakamoto_tenure_info(
                    peerhost,
                )));
            }
            NakamotoUnconfirmedDownloadState::GetTenureStartBlock(block_id) => {
                return Ok(Some(StacksHttpRequest::new_get_nakamoto_block(
                    peerhost,
                    block_id.clone(),
                )));
            }
            NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(tip_block_id) => {
                return Ok(Some(StacksHttpRequest::new_get_nakamoto_tenure(
                    peerhost,
                    tip_block_id.clone(),
                    self.highest_processed_block_id.clone(),
                )));
            }
            NakamotoUnconfirmedDownloadState::Done => {
                // got all unconfirmed blocks!  Next step is to turn this downloader into a confirmed
                // tenure downloader using the earliest unconfirmed tenure block.
                return Ok(None);
            }
        }
    }

    /// Begin the next download request for this state machine.
    /// Returns Ok(true) if we sent the request, or there's already an in-flight request
    /// Returns Ok(false) if not (e.g. neighbor is known to be dead or broken)
    pub fn send_next_download_request(
        &self,
        network: &mut PeerNetwork,
        neighbor_rpc: &mut NeighborRPC,
    ) -> Result<bool, NetError> {
        if neighbor_rpc.has_inflight(&self.naddr) {
            test_debug!("Peer {} has an inflight request", &self.naddr);
            return Ok(true);
        }
        if neighbor_rpc.is_dead_or_broken(network, &self.naddr) {
            return Err(NetError::PeerNotConnected);
        }

        let Some(peerhost) = NeighborRPC::get_peer_host(network, &self.naddr) else {
            // no conversation open to this neighbor
            neighbor_rpc.add_dead(network, &self.naddr);
            return Err(NetError::PeerNotConnected);
        };

        let request = match self.make_next_download_request(peerhost) {
            Ok(Some(request)) => request,
            Ok(None) => {
                return Ok(true);
            }
            Err(_) => {
                return Ok(false);
            }
        };

        neighbor_rpc.send_request(network, self.naddr.clone(), request)?;
        Ok(true)
    }

    /// Handle a received StacksHttpResponse.
    /// If we get the full tenure, return it.
    pub fn handle_next_download_response(
        &mut self,
        response: StacksHttpResponse,
        sortdb: &SortitionDB,
        sort_tip: &BlockSnapshot,
        chainstate: &StacksChainState,
        agg_pubkeys: &BTreeMap<u64, Option<Point>>,
    ) -> Result<Option<Vec<NakamotoBlock>>, NetError> {
        match &self.state {
            NakamotoUnconfirmedDownloadState::GetTenureInfo => {
                test_debug!("Got tenure-info response");
                let tenure_info = response.decode_nakamoto_tenure_info()?;
                test_debug!("Got tenure-info response: {:?}", &tenure_info);
                self.try_accept_tenure_info(
                    sortdb,
                    sort_tip,
                    chainstate,
                    tenure_info,
                    agg_pubkeys,
                )?;
                Ok(None)
            }
            NakamotoUnconfirmedDownloadState::GetTenureStartBlock(..) => {
                test_debug!("Got tenure start-block response");
                let block = response.decode_nakamoto_block()?;
                self.try_accept_unconfirmed_tenure_start_block(block)?;
                Ok(None)
            }
            NakamotoUnconfirmedDownloadState::GetUnconfirmedTenureBlocks(..) => {
                test_debug!("Got unconfirmed tenure blocks response");
                let blocks = response.decode_nakamoto_tenure()?;
                self.try_accept_unconfirmed_tenure_blocks(blocks)
            }
            NakamotoUnconfirmedDownloadState::Done => {
                return Err(NetError::InvalidState);
            }
        }
    }

    pub fn is_done(&self) -> bool {
        self.state == NakamotoUnconfirmedDownloadState::Done
    }
}

/// A tenure that this node wants.
#[derive(Debug, PartialEq, Clone)]
pub(crate) struct WantedTenure {
    /// Consensus hash that identifies the start of the tenure
    pub tenure_id_consensus_hash: ConsensusHash,
    /// Winning block-commit block ID for this tenure's snapshot (NOTE THAT THIS IS NOT THE
    /// TENURE-START BLOCK FOR THIS TENURE).
    pub winning_block_id: StacksBlockId,
    /// burnchain block height of this tenure ID consensus hash
    pub burn_height: u64,
    /// Whether or not this tenure has been acted upon (i.e. set to true if there's no need to
    /// download it)
    pub processed: bool,
}

impl WantedTenure {
    pub fn new(
        tenure_id_consensus_hash: ConsensusHash,
        winning_block_id: StacksBlockId,
        burn_height: u64,
    ) -> Self {
        Self {
            tenure_id_consensus_hash,
            winning_block_id,
            burn_height,
            processed: false,
        }
    }
}

/// A tenure's start and end blocks
#[derive(Debug, PartialEq, Clone)]
pub(crate) struct TenureStartEnd {
    /// Consensus hash that identifies the start of the tenure
    pub tenure_id_consensus_hash: ConsensusHash,
    /// Tenure-start block ID
    pub start_block_id: StacksBlockId,
    /// Last block ID
    pub end_block_id: StacksBlockId,
    /// Whether or not to fetch the end-block-id directly
    pub fetch_end_block: bool,
    /// Reward cycle of the start block
    pub start_reward_cycle: u64,
    /// Reward cycle of the end block
    pub end_reward_cycle: u64,
    /// Whether or not this tenure has been processed
    pub processed: bool,
}

pub(crate) type AvailableTenures = HashMap<ConsensusHash, TenureStartEnd>;

impl TenureStartEnd {
    pub fn new(
        tenure_id_consensus_hash: ConsensusHash,
        start_block_id: StacksBlockId,
        end_block_id: StacksBlockId,
        start_reward_cycle: u64,
        end_reward_cycle: u64,
        processed: bool,
    ) -> Self {
        Self {
            tenure_id_consensus_hash,
            start_block_id,
            end_block_id,
            start_reward_cycle,
            end_reward_cycle,
            fetch_end_block: false,
            processed,
        }
    }

    /// Given a list of wanted tenures and a peer's inventory bitvectors over the same range of
    /// tenures, calculate the list of start/end blocks for each wanted tenure.
    ///
    /// Recall that in Nakamoto, a block-commit commits to the parent tenure's first block.  So if
    /// bit i is set (i.e. `wanted_tenures[i]` has tenure data), then it really means that the tenure
    /// start block is the winning block hash in the _subsequent_ `wanted_tenures` list item for which
    /// its corresponding bit is 1.  Similarly, the end block is the winning block hash in the
    /// `wanted_tenures` list item _after that_ whose bit is 1.
    ///
    /// As such, this algorithm needs to search not only the wanted tenures and inventories for
    /// this reward cycle, but also the next.
    ///
    /// The `wanted_tenures` and `next_wanted_tenures` values must be aligned to reward cycle
    /// boundaries (mod 0).  The code uses this assumption to assign reward cycles to blocks in the
    /// `TenureStartEnd`s in the returned `AvailableTenures` map.
    ///
    /// Returns the set of available tenures for all tenures in `wanted_tenures` that can be found
    /// with the available information.
    /// Returns None if there is no inventory data for the given reward cycle.
    pub fn from_inventory(
        rc: u64,
        wanted_tenures: &[WantedTenure],
        next_wanted_tenures: Option<&[WantedTenure]>,
        pox_constants: &PoxConstants,
        first_burn_height: u64,
        invs: &NakamotoTenureInv,
    ) -> Option<AvailableTenures> {
        // if bit i is set, that means that the tenure data for the ith tenure in the sortition
        // history was present.  But given that block-commits commit to the start block of the
        // parent tenure, the start-block ID for tenure i would be the StacksBlockId for the
        // next-available tenure.  Its end-block ID would be the StacksBlockId for the
        // next-available tenure after that.
        let invbits = invs.tenures_inv.get(&rc)?;
        let mut tenure_block_ids = AvailableTenures::new();
        let mut i = 0;
        let mut last_tenure = 0;
        let mut last_tenure_ch = None;
        while i < wanted_tenures.len() {
            let Some(wt) = wanted_tenures.get(i) else {
                test_debug!("i={} no wanted tenure", i);
                break;
            };

            // advance to next tenure-start sortition
            let bit = u16::try_from(i).expect("FATAL: more sortitions than u16::MAX");
            if !invbits.get(bit).unwrap_or(false) {
                test_debug!("i={} bit not set", i);
                i += 1;
                continue;
            }

            // the last tenure we'll consider
            last_tenure = i;

            // find next 1-bit -- corresponds to tenure-start block ID
            loop {
                i += 1;
                if i >= wanted_tenures.len() {
                    test_debug!("i={} out of wanted_tenures", i);
                    break;
                }
                let bit = u16::try_from(i).expect("FATAL: more sortitions than u16::MAX");
                if !invbits.get(bit).unwrap_or(false) {
                    test_debug!("i={} start block bit not set", i);
                    continue;
                }

                // i now points to the item in wanted_tenures with the tenure-start block ID for
                // `wt`
                break;
            }
            let Some(wt_start) = wanted_tenures.get(i) else {
                test_debug!("i={} no start wanted tenure", i);
                break;
            };

            // find the next 1-bit after that -- corresponds to the tenure-end block ID.
            // `j` points to the first tenure in `wanted_tenures` after `wanted_tenures[i]` that
            // corresponds to a tenure-start (according to the inv)
            let mut j = i;
            loop {
                j += 1;
                if j >= wanted_tenures.len() {
                    test_debug!("i={}, j={} out of wanted_tenures", i, j);
                    break;
                }

                let bit = u16::try_from(j).expect("FATAL: more sortitions than u16::MAX");
                if !invbits.get(bit).unwrap_or(false) {
                    test_debug!("i={}, j={} end block bit not set", i, j);
                    continue;
                }

                // j now points to the item in wanted_tenures with the tenure-send block ID for
                // `ch`.
                break;
            }
            let Some(wt_end) = wanted_tenures.get(j) else {
                test_debug!("i={}, j={} no end wanted tenure", i, j);
                break;
            };

            let tenure_start_end = TenureStartEnd::new(
                wt.tenure_id_consensus_hash.clone(),
                wt_start.winning_block_id.clone(),
                wt_end.winning_block_id.clone(),
                rc,
                rc,
                wt.processed,
            );
            test_debug!(
                "i={}, j={}, len={}; {:?}",
                i,
                j,
                wanted_tenures.len(),
                &tenure_start_end
            );
            last_tenure_ch = Some(wt.tenure_id_consensus_hash.clone());
            tenure_block_ids.insert(wt.tenure_id_consensus_hash.clone(), tenure_start_end);
            i = last_tenure + 1;
        }

        let Some(next_wanted_tenures) = next_wanted_tenures else {
            // nothing more to do
            test_debug!("No next_wanted_tenures");
            return Some(tenure_block_ids);
        };

        // `wanted_tenures` was a full reward cycle, so be sure to fetch the tenure-end block of
        // the last tenure derived from it
        if let Some(last_tenure_ch) = last_tenure_ch.take() {
            if let Some(last_tenure) = tenure_block_ids.get_mut(&last_tenure_ch) {
                test_debug!(
                    "Will directly fetch end-block {} for tenure {}",
                    &last_tenure.end_block_id,
                    &last_tenure.tenure_id_consensus_hash
                );
                last_tenure.fetch_end_block = true;
            }
        }

        let Some(next_invbits) = invs.tenures_inv.get(&rc.saturating_add(1)) else {
            // nothing more to do
            test_debug!("no inventory for cycle {}", rc.saturating_add(1));
            return Some(tenure_block_ids);
        };

        // proceed to find availability until each tenure in `wanted_tenures` is accounted for,
        // using `next_wanted_tenures`
        i = last_tenure;

        // once again, `i` will be bumped from the last-considered tenure to the tenure's start
        // block sortition.
        // here, `n` indexes `next_wanted_tenures` in the event that the start block for tenure `i`
        // is not present in `wanted_tenures`.
        let mut n = 0;

        // whether or not `n` is used to index into `next_wanted_tenures`
        let mut next = false;
        while i < wanted_tenures.len() {
            let Some(wt) = wanted_tenures.get(i) else {
                break;
            };
            test_debug!(
                "consider next wanted tenure which starts with i={} {:?}",
                i,
                &wt
            );

            // advance to next tenure-start sortition
            let bit = u16::try_from(i).expect("FATAL: more sortitions than u16::MAX");
            if !invbits.get(bit).unwrap_or(false) {
                i += 1;
                continue;
            }

            // find next 1-bit -- corresponds to tenure-start block ID.
            // It could be in `wanted_tenures`, or it could be in `next_wanted_tenures`.  Search
            // both.
            loop {
                if i < wanted_tenures.len() {
                    // still searching `wanted_tenures`
                    i += 1;
                    if i >= wanted_tenures.len() {
                        // switch over to `next_wanted_tenures`
                        continue;
                    }
                    let bit = u16::try_from(i).expect("FATAL: more sortitions than u16::MAX");
                    if !invbits.get(bit).unwrap_or(false) {
                        continue;
                    }

                    // i now points to the item in wanted_tenures with the tenure-start block ID for
                    // `wt`.
                    // n does not point to anything
                    test_debug!(
                        "next wanted tenure start block at current i={} {:?}",
                        i,
                        &wanted_tenures[i]
                    );
                    break;
                } else {
                    // searching `next_wanted_tenures`
                    if n >= next_wanted_tenures.len() {
                        break;
                    }
                    let bit = u16::try_from(n).expect("FATAL: more sortitions than u16::MAX");
                    if !next_invbits.get(bit).unwrap_or(false) {
                        n += 1;
                        continue;
                    }

                    // n now points to the item in next_wanted_tenures with the tenure-start block ID for
                    // `wt`
                    next = true;
                    test_debug!(
                        "next wanted tenure start block at next n={} {:?}",
                        n,
                        &next_wanted_tenures[n]
                    );
                    break;
                }
            }
            let wt_start = if i < wanted_tenures.len() {
                let Some(wt) = wanted_tenures.get(i) else {
                    break;
                };
                wt
            } else {
                let Some(wt) = next_wanted_tenures.get(n) else {
                    break;
                };
                wt
            };
            test_debug!("next start tenure is {:?}", &wt_start);

            // find the next 1-bit after that -- corresponds to the tenure-end block ID.
            // `k` necessarily points the tenure in `next_wanted_tenures` which corresponds to the
            // tenure after the previously-found tenure (either `wanted_tenures[i]` or
            // `next_wanted_tenures[n]`, depending on the blockchain structure).
            let mut k = if next {
                // start block is in `next_wanted_tenures` (at `n`), so search for the wanted
                // tenure whose bit is after `n`
                n + 1
            } else {
                // start block is in `wanted_tenures`, and it's the last tenure that has a 1-bit in
                // `wanted_tenures`. Start searching `next_wanted_tenures`.
                0
            };

            while k < next_wanted_tenures.len() {
                let bit = u16::try_from(k).expect("FATAL: more sortitions than u16::MAX");
                if !next_invbits.get(bit).unwrap_or(false) {
                    k += 1;
                    continue;
                }

                // k now points to the item in wanted_tenures with the tenure-send block ID for
                // `ch`.
                test_debug!("next end tenure is k={} {:?}", k, &next_wanted_tenures[k]);
                break;
            }
            let Some(wt_end) = next_wanted_tenures.get(k) else {
                break;
            };

            let mut tenure_start_end = TenureStartEnd::new(
                wt.tenure_id_consensus_hash.clone(),
                wt_start.winning_block_id.clone(),
                wt_end.winning_block_id.clone(),
                rc,
                pox_constants
                    .block_height_to_reward_cycle(first_burn_height, wt_start.burn_height)
                    .expect("FATAL: tenure from before system start"),
                wt.processed,
            );
            tenure_start_end.fetch_end_block = true;
            test_debug!(
                "i={}, k={}, n={}, len={}, next_len={}; {:?}",
                i,
                k,
                n,
                wanted_tenures.len(),
                next_wanted_tenures.len(),
                &tenure_start_end
            );
            tenure_block_ids.insert(wt.tenure_id_consensus_hash.clone(), tenure_start_end);
        }
        Some(tenure_block_ids)
    }
}

/// The overall downloader can operate in one of two states:
/// * it's doing IBD, in which case it's downloading tenures using neighbor inventories and
/// the start/end block ID hashes obtained from block-commits.  This works up until the last two
/// tenures.
/// * it's in steady-state, in which case it's downloading the last two tenures from its neighbors.
#[derive(Debug, Clone, PartialEq)]
pub enum NakamotoDownloadState {
    /// confirmed tenure download
    Confirmed,
    /// unconfirmed tenure download
    Unconfirmed,
}

impl fmt::Display for NakamotoDownloadState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// A set of confirmed downloader state machines assigned to one or more neighbors
pub struct NakamotoTenureDownloaderSet {
    downloaders: Vec<Option<NakamotoTenureDownloader>>,
    peers: HashMap<NeighborAddress, usize>,
    completed_tenures: HashSet<ConsensusHash>,
}

impl NakamotoTenureDownloaderSet {
    pub fn new() -> Self {
        Self {
            downloaders: vec![],
            peers: HashMap::new(),
            completed_tenures: HashSet::new(),
        }
    }

    fn add_downloader(&mut self, naddr: NeighborAddress, downloader: NakamotoTenureDownloader) {
        test_debug!(
            "Add downloader for tenure {} driven by {}",
            &downloader.tenure_id_consensus_hash,
            &naddr
        );
        if let Some(idx) = self.peers.get(&naddr) {
            self.downloaders[*idx] = Some(downloader);
        } else {
            self.downloaders.push(Some(downloader));
            self.peers.insert(naddr, self.downloaders.len() - 1);
        }
    }

    fn has_downloader(&self, naddr: &NeighborAddress) -> bool {
        let Some(idx) = self.peers.get(naddr) else {
            return false;
        };
        let Some(downloader_opt) = self.downloaders.get(*idx) else {
            return false;
        };
        downloader_opt.is_some()
    }

    pub fn clear_downloader(&mut self, naddr: &NeighborAddress) {
        let Some(index) = self.peers.remove(naddr) else {
            return;
        };
        self.downloaders[index] = None;
    }

    pub(crate) fn add_downloaders(
        &mut self,
        iter: impl IntoIterator<Item = (NeighborAddress, NakamotoTenureDownloader)>,
    ) {
        for (naddr, downloader) in iter {
            if self.has_downloader(&naddr) {
                test_debug!("Already have downloader for {}", &naddr);
                continue;
            }
            self.add_downloader(naddr, downloader);
        }
    }

    pub fn inflight(&self) -> usize {
        let mut cnt = 0;
        for downloader_opt in self.downloaders.iter() {
            let Some(downloader) = downloader_opt else {
                continue;
            };
            if downloader.idle {
                continue;
            }
            if downloader.is_waiting() {
                continue;
            }
            if downloader.is_done() {
                continue;
            }
            cnt += 1;
        }
        cnt
    }

    pub fn is_tenure_inflight(&self, ch: &ConsensusHash) -> bool {
        for downloader_opt in self.downloaders.iter() {
            let Some(downloader) = downloader_opt else {
                continue;
            };
            if &downloader.tenure_id_consensus_hash == ch {
                return true;
            }
        }
        false
    }

    pub fn is_empty(&self) -> bool {
        self.inflight() == 0
    }

    pub fn try_resume_peer(&mut self, naddr: NeighborAddress) -> bool {
        if let Some(idx) = self.peers.get(&naddr) {
            let Some(Some(_downloader)) = self.downloaders.get(*idx) else {
                return false;
            };

            test_debug!(
                "Peer {} already bound to downloader for {}",
                &naddr,
                &_downloader.tenure_id_consensus_hash
            );
            return true;
        }
        for (i, downloader_opt) in self.downloaders.iter_mut().enumerate() {
            let Some(downloader) = downloader_opt else {
                continue;
            };
            if !downloader.idle {
                continue;
            }
            if downloader.is_waiting() {
                continue;
            }
            if downloader.naddr != naddr {
                continue;
            }
            test_debug!(
                "Assign peer {} to work on downloader for {} in state {}",
                &naddr,
                &downloader.tenure_id_consensus_hash,
                &downloader.state
            );
            self.peers.insert(naddr, i);
            return true;
        }
        return false;
    }

    pub fn clear_available_peers(&mut self) {
        let mut idled: Vec<NeighborAddress> = vec![];
        for (naddr, i) in self.peers.iter() {
            let Some(downloader_opt) = self.downloaders.get(*i) else {
                // should be unreachable
                idled.push(naddr.clone());
                continue;
            };
            let Some(downloader) = downloader_opt else {
                test_debug!("Remove peer {} for null download {}", &naddr, i);
                idled.push(naddr.clone());
                continue;
            };
            if downloader.idle || downloader.is_waiting() {
                test_debug!(
                    "Remove idled peer {} for tenure download {}",
                    &naddr,
                    &downloader.tenure_id_consensus_hash
                );
                idled.push(naddr.clone());
            }
        }
        for naddr in idled.into_iter() {
            self.peers.remove(&naddr);
        }
    }

    pub fn clear_finished_downloaders(&mut self) {
        for downloader_opt in self.downloaders.iter_mut() {
            let Some(downloader) = downloader_opt else {
                continue;
            };
            if downloader.is_done() {
                *downloader_opt = None;
            }
        }
    }

    pub(crate) fn find_new_tenure_start_blocks(&self) -> HashMap<StacksBlockId, NakamotoBlock> {
        let mut ret = HashMap::new();
        for downloader_opt in self.downloaders.iter() {
            let Some(downloader) = downloader_opt else {
                continue;
            };
            let Some(block) = downloader.tenure_start_block.as_ref() else {
                continue;
            };
            ret.insert(block.block_id(), block.clone());
        }
        ret
    }

    pub(crate) fn handle_tenure_end_blocks(
        &mut self,
        tenure_start_blocks: &HashMap<StacksBlockId, NakamotoBlock>,
    ) -> Vec<NeighborAddress> {
        test_debug!(
            "handle tenure-end blocks: {:?}",
            &tenure_start_blocks.keys().collect::<Vec<_>>()
        );
        let mut dead = vec![];
        for downloader_opt in self.downloaders.iter_mut() {
            let Some(downloader) = downloader_opt else {
                continue;
            };
            let NakamotoTenureDownloadState::WaitForTenureEndBlock(end_block_id) =
                &downloader.state
            else {
                continue;
            };
            let Some(end_block) = tenure_start_blocks.get(end_block_id) else {
                continue;
            };
            if let Err(e) = downloader.try_accept_tenure_end_block(end_block) {
                warn!(
                    "Failed to accept tenure end-block {} for tenure {}: {:?}",
                    &end_block.block_id(),
                    &downloader.tenure_id_consensus_hash,
                    &e
                );
                dead.push(downloader.naddr.clone());
            }
        }
        dead
    }

    pub(crate) fn has_downloader_for_tenure(&self, tenure_id: &ConsensusHash) -> bool {
        for downloader_opt in self.downloaders.iter() {
            let Some(downloader) = downloader_opt else {
                continue;
            };
            if &downloader.tenure_id_consensus_hash == tenure_id {
                test_debug!(
                    "Have downloader for tenure {} already (idle={}, waiting={}, state={})",
                    tenure_id,
                    downloader.idle,
                    downloader.is_waiting(),
                    &downloader.state
                );
                return true;
            }
        }
        false
    }

    /// In the event that the last confirmed tenure in a reward cycle contains the PoX anchor
    /// block, we need to go and directly fetch its end block instead of waiting for another
    /// NakamotoTenureDownloader to provide it as its tenure-start block.  Naively, this method
    /// just unconditionally sets the highest available tenure downloader to fetch its tenure end block.
    pub(crate) fn try_transition_fetch_tenure_end_blocks(
        &mut self,
        tenure_block_ids: &HashMap<NeighborAddress, AvailableTenures>,
    ) {
        // find tenures in which we need to fetch the tenure-end block directly.
        let mut last_available_tenures: HashSet<StacksBlockId> = HashSet::new();
        for (_, all_available) in tenure_block_ids.iter() {
            for (_, available) in all_available.iter() {
                if available.fetch_end_block {
                    last_available_tenures.insert(available.end_block_id.clone());
                }
            }
        }

        // is anyone downloading this tenure, and if so, are they waiting?  If so, then flip to
        // fetching
        for downloader_opt in self.downloaders.iter_mut() {
            let Some(downloader) = downloader_opt.as_mut() else {
                continue;
            };
            if !downloader.idle {
                continue;
            }
            if !downloader.is_waiting() {
                continue;
            }
            if !last_available_tenures.contains(&downloader.tenure_end_block_id) {
                continue;
            }
            test_debug!(
                "Transition downloader for {} from waiting to fetching",
                &downloader.tenure_id_consensus_hash
            );
            if let Err(e) = downloader.transition_to_fetch_end_block() {
                warn!(
                    "Downloader for {} failed to transition to fetch end block: {:?}",
                    &downloader.tenure_id_consensus_hash, &e
                );
            }
        }
    }

    /// Create a given number of downloads from a schedule and availability set.
    /// Removes items from the schedule, and neighbors from the availability set.
    /// A neighbor will be issued at most one request.
    pub(crate) fn make_tenure_downloaders(
        &mut self,
        schedule: &mut VecDeque<ConsensusHash>,
        available: &mut HashMap<ConsensusHash, Vec<NeighborAddress>>,
        tenure_block_ids: &HashMap<NeighborAddress, AvailableTenures>,
        count: usize,
        agg_public_keys: &BTreeMap<u64, Option<Point>>,
    ) {
        test_debug!("schedule: {:?}", schedule);
        test_debug!("available: {:?}", &available);
        test_debug!("tenure_block_ids: {:?}", &tenure_block_ids);
        test_debug!("inflight: {}", self.inflight());

        self.clear_available_peers();
        self.clear_finished_downloaders();
        self.try_transition_fetch_tenure_end_blocks(tenure_block_ids);
        while self.inflight() < count {
            let Some(ch) = schedule.front() else {
                break;
            };
            if self.completed_tenures.contains(&ch) {
                test_debug!("Already successfully downloaded tenure {}", &ch);
                schedule.pop_front();
                continue;
            }
            let Some(neighbors) = available.get_mut(ch) else {
                // not found on any neighbors, so stop trying this tenure
                test_debug!("No neighbors have tenure {}", ch);
                schedule.pop_front();
                continue;
            };
            if neighbors.len() == 0 {
                // no more neighbors to try
                test_debug!("No more neighbors can serve tenure {}", ch);
                schedule.pop_front();
                continue;
            }
            let Some(naddr) = neighbors.pop() else {
                test_debug!("No more neighbors can serve tenure {}", ch);
                schedule.pop_front();
                continue;
            };
            if self.try_resume_peer(naddr.clone()) {
                continue;
            };
            if self.has_downloader_for_tenure(&ch) {
                schedule.pop_front();
                continue;
            }

            let Some(available_tenures) = tenure_block_ids.get(&naddr) else {
                // this peer doesn't have any known tenures, so try the others
                test_debug!("No tenures available from {}", &naddr);
                continue;
            };
            let Some(tenure_info) = available_tenures.get(ch) else {
                // this peer does not have a tenure start/end block for this tenure, so try the
                // others.
                test_debug!("Neighbor {} does not serve tenure {}", &naddr, ch);
                continue;
            };
            let Some(Some(start_agg_pubkey)) = agg_public_keys.get(&tenure_info.start_reward_cycle)
            else {
                test_debug!(
                    "Cannot fetch tenure-start block due to no known aggregate public key: {:?}",
                    &tenure_info
                );
                schedule.pop_front();
                continue;
            };
            let Some(Some(end_agg_pubkey)) = agg_public_keys.get(&tenure_info.end_reward_cycle)
            else {
                test_debug!(
                    "Cannot fetch tenure-end block due to no known aggregate public key: {:?}",
                    &tenure_info
                );
                schedule.pop_front();
                continue;
            };

            test_debug!(
                "Download tenure {} (start={}, end={}) with aggregate keys {}, {} (rc {},{})",
                &ch,
                &tenure_info.start_block_id,
                &tenure_info.end_block_id,
                &start_agg_pubkey,
                &end_agg_pubkey,
                tenure_info.start_reward_cycle,
                tenure_info.end_reward_cycle
            );
            let tenure_download = NakamotoTenureDownloader::new(
                ch.clone(),
                tenure_info.start_block_id.clone(),
                tenure_info.end_block_id.clone(),
                naddr.clone(),
                start_agg_pubkey.clone(),
                end_agg_pubkey.clone(),
            );

            test_debug!("Request tenure {} from neighbor {}", ch, &naddr);
            self.add_downloader(naddr, tenure_download);
            schedule.pop_front();
        }
    }

    /// Run all confirmed downloaders.  Remove dead downloaders.
    /// Returns the set of downloaded blocks
    pub fn run(
        &mut self,
        network: &mut PeerNetwork,
        neighbor_rpc: &mut NeighborRPC,
    ) -> HashMap<ConsensusHash, Vec<NakamotoBlock>> {
        let addrs: Vec<_> = self.peers.keys().map(|addr| addr.clone()).collect();
        let mut finished = vec![];
        let mut finished_tenures = vec![];
        let mut new_blocks = HashMap::new();

        // send requests
        for (naddr, index) in self.peers.iter() {
            if neighbor_rpc.has_inflight(&naddr) {
                test_debug!("Peer {} has an inflight request", &naddr);
                continue;
            }
            let Some(Some(downloader)) = self.downloaders.get_mut(*index) else {
                test_debug!("No downloader for {}", &naddr);
                continue;
            };
            if downloader.is_done() {
                test_debug!("Downloader for {} is done", &naddr);
                finished.push(naddr.clone());
                finished_tenures.push(downloader.tenure_id_consensus_hash.clone());
                continue;
            }
            test_debug!(
                "Send request to {} for tenure {} (state {})",
                &naddr,
                &downloader.tenure_id_consensus_hash,
                &downloader.state
            );
            let Ok(sent_opt) = downloader.send_next_download_request(network, neighbor_rpc) else {
                test_debug!("Downloader for {} failed; this peer is dead", &naddr);
                neighbor_rpc.add_dead(network, naddr);
                continue;
            };
            if let Some(sent) = sent_opt {
                if !sent {
                    // this downloader is dead or broken
                    finished.push(naddr.clone());
                    continue;
                }
            } else {
                // this downloader is blocked
                continue;
            }
        }

        // clear dead, broken, and done
        for naddr in addrs.iter() {
            if neighbor_rpc.is_dead_or_broken(network, naddr) {
                test_debug!("Remove dead/broken downloader for {}", &naddr);
                self.clear_downloader(&naddr);
            }
        }
        for done_naddr in finished.iter() {
            test_debug!("Remove finished downloader for {}", &done_naddr);
            self.clear_downloader(&done_naddr);
        }
        for done_tenure in finished_tenures.iter() {
            self.completed_tenures.insert(done_tenure.clone());
        }
        finished.clear();
        finished_tenures.clear();

        // handle responses
        for (naddr, response) in neighbor_rpc.collect_replies(network) {
            let Some(index) = self.peers.get(&naddr) else {
                test_debug!("No downloader for {}", &naddr);
                continue;
            };
            let Some(Some(downloader)) = self.downloaders.get_mut(*index) else {
                test_debug!("No downloader for {}", &naddr);
                continue;
            };
            test_debug!("Got response from {}", &naddr);

            let Ok(blocks_opt) = downloader.handle_next_download_response(response) else {
                test_debug!("Failed to handle download response from {}", &naddr);
                neighbor_rpc.add_dead(network, &naddr);
                continue;
            };

            let Some(blocks) = blocks_opt else {
                continue;
            };

            test_debug!(
                "Got {} blocks for tenure {}",
                blocks.len(),
                &downloader.tenure_id_consensus_hash
            );
            new_blocks.insert(downloader.tenure_id_consensus_hash.clone(), blocks);
            if downloader.is_done() {
                finished.push(naddr.clone());
                finished_tenures.push(downloader.tenure_id_consensus_hash.clone());
                continue;
            }
        }

        // clear dead, broken, and done
        for naddr in addrs.iter() {
            if neighbor_rpc.is_dead_or_broken(network, naddr) {
                test_debug!("Remove dead/broken downloader for {}", &naddr);
                self.clear_downloader(naddr);
            }
        }
        for done_naddr in finished.iter() {
            test_debug!("Remove finished downloader for {}", &done_naddr);
            self.clear_downloader(done_naddr);
        }
        for done_tenure in finished_tenures.iter() {
            self.completed_tenures.insert(done_tenure.clone());
        }

        new_blocks
    }
}

pub struct NakamotoDownloadStateMachine {
    /// What's the start burn block height for Nakamoto?
    nakamoto_start_height: u64,
    /// What's the current reward cycle we're tracking?
    pub(crate) reward_cycle: u64,
    /// List of (possible) tenures in the current reward cycle
    pub(crate) wanted_tenures: Vec<WantedTenure>,
    /// List of (possible) tenures in the previous reward cycle. Will be None in the first reward
    /// cycle of Nakamoto
    pub(crate) prev_wanted_tenures: Option<Vec<WantedTenure>>,
    /// Last burnchain tip we've seen
    last_sort_tip: Option<BlockSnapshot>,
    /// Download behavior we're in
    state: NakamotoDownloadState,
    /// Map a tenure ID to its tenure start-block and end-block for each of our neighbors' invs
    tenure_block_ids: HashMap<NeighborAddress, AvailableTenures>,
    /// Who can serve a given tenure
    available_tenures: HashMap<ConsensusHash, Vec<NeighborAddress>>,
    /// Confirmed tenure download schedule
    tenure_download_schedule: VecDeque<ConsensusHash>,
    /// Unconfirmed tenure download schedule
    unconfirmed_tenure_download_schedule: VecDeque<NeighborAddress>,
    /// Ongoing unconfirmed tenure downloads, prioritized in who announces the latest block
    unconfirmed_tenure_downloads: HashMap<NeighborAddress, NakamotoUnconfirmedTenureDownloader>,
    /// Ongoing confirmed tenure downloads for when we know the start and end block hashes.
    tenure_downloads: NakamotoTenureDownloaderSet,
    /// resolved tenure-start blocks
    tenure_start_blocks: HashMap<StacksBlockId, NakamotoBlock>,
    /// comms to remote neighbors
    neighbor_rpc: NeighborRPC,
}

impl NakamotoDownloadStateMachine {
    pub fn new(nakamoto_start_height: u64) -> Self {
        Self {
            nakamoto_start_height,
            reward_cycle: 0, // will be calculated at runtime
            wanted_tenures: vec![],
            prev_wanted_tenures: None,
            last_sort_tip: None,
            state: NakamotoDownloadState::Confirmed,
            tenure_block_ids: HashMap::new(),
            available_tenures: HashMap::new(),
            tenure_download_schedule: VecDeque::new(),
            unconfirmed_tenure_download_schedule: VecDeque::new(),
            tenure_downloads: NakamotoTenureDownloaderSet::new(),
            unconfirmed_tenure_downloads: HashMap::new(),
            tenure_start_blocks: HashMap::new(),
            neighbor_rpc: NeighborRPC::new(),
        }
    }

    /// Get a range of wanted tenures
    /// Does not set the .processed bits.
    /// Returns the tenures from first_block_height (inclusive) to last_block_height (exclusive)
    pub(crate) fn load_wanted_tenures(
        ih: &SortitionHandleConn,
        first_block_height: u64,
        last_block_height: u64,
    ) -> Result<Vec<WantedTenure>, NetError> {
        let mut wanted_tenures = Vec::with_capacity(
            usize::try_from(last_block_height.saturating_sub(first_block_height))
                .expect("FATAL: infallible: usize can't old a reward cycle"),
        );
        let mut cursor = ih
            .get_block_snapshot_by_height(last_block_height.saturating_sub(1))?
            .ok_or(DBError::NotFoundError)?;
        while cursor.block_height >= first_block_height {
            test_debug!(
                "Load sortition {}/{} burn height {}",
                &cursor.consensus_hash,
                &cursor.winning_stacks_block_hash,
                cursor.block_height
            );
            wanted_tenures.push(WantedTenure::new(
                cursor.consensus_hash,
                StacksBlockId(cursor.winning_stacks_block_hash.0),
                cursor.block_height,
            ));
            cursor = SortitionDB::get_block_snapshot(&ih, &cursor.parent_sortition_id)?
                .ok_or(DBError::NotFoundError)?;
        }
        wanted_tenures.reverse();
        Ok(wanted_tenures)
    }

    /// Find the list of wanted tenures and processed tenures for a given complete reward cycle
    /// (i.e. not the one at the burnchain tip).  Used only in IBD.
    ///
    /// Returns
    /// * list of (consensus hash, tenure-start block ID of parent tenure) ordered by sortition
    /// * set of tenure ID consensus hashes for tenures we already have processed
    ///
    /// Returns None if `tip.block_height` matches `burnchain_block`
    pub(crate) fn load_wanted_tenures_for_reward_cycle(
        cur_rc: u64,
        tip: &BlockSnapshot,
        sortdb: &SortitionDB,
    ) -> Result<Vec<WantedTenure>, NetError> {
        // careful -- need .saturating_sub(1) since this calculation puts the reward cycle start at
        // block height 1 mod reward cycle len, but we really want 0 mod reward cycle len
        let first_block_height = sortdb
            .pox_constants
            .reward_cycle_to_block_height(sortdb.first_block_height, cur_rc)
            .saturating_sub(1);
        let last_block_height = sortdb
            .pox_constants
            .reward_cycle_to_block_height(sortdb.first_block_height, cur_rc.saturating_add(1))
            .saturating_sub(1);

        test_debug!(
            "Load reward cycle sortitions between {} and {} (rc is {})",
            first_block_height,
            last_block_height,
            cur_rc
        );

        // find all sortitions in this reward cycle
        let ih = sortdb.index_handle(&tip.sortition_id);
        Self::load_wanted_tenures(&ih, first_block_height, last_block_height)
    }

    /// Update the list of wanted tenures and processed tenures for a given reward cycle.
    ///
    /// `wanted_tenures` needs to be sorted by block height.
    ///
    pub(crate) fn update_wanted_tenures_for_reward_cycle(
        cur_rc: u64,
        tip: &BlockSnapshot,
        sortdb: &SortitionDB,
        wanted_tenures: &mut Vec<WantedTenure>,
    ) -> Result<(), NetError> {
        let highest_tenure_height = wanted_tenures.last().map(|wt| wt.burn_height).unwrap_or(0);

        // careful -- need .saturating_sub(1) since this calculation puts the reward cycle start at
        // block height 1 mod reward cycle len, but we really want 0 mod reward cycle len
        let first_block_height = sortdb
            .pox_constants
            .reward_cycle_to_block_height(sortdb.first_block_height, cur_rc)
            .saturating_sub(1)
            .max(highest_tenure_height.saturating_add(1));

        let last_block_height = sortdb
            .pox_constants
            .reward_cycle_to_block_height(sortdb.first_block_height, cur_rc.saturating_add(1))
            .saturating_sub(1)
            .min(tip.block_height.saturating_add(1));

        if highest_tenure_height > last_block_height {
            test_debug!(
                "Will NOT update wanted tenures for reward cycle {}: {} > {}",
                cur_rc,
                highest_tenure_height,
                last_block_height
            );
            return Ok(());
        }

        test_debug!(
            "Update reward cycle sortitions between {} and {} (rc is {})",
            first_block_height,
            last_block_height,
            cur_rc
        );

        // find all sortitions in this reward cycle
        let ih = sortdb.index_handle(&tip.sortition_id);
        let mut new_tenures =
            Self::load_wanted_tenures(&ih, first_block_height, last_block_height)?;
        wanted_tenures.append(&mut new_tenures);
        Ok(())
    }

    /// Update an existing list of wanted tenures and processed tenures for the chain tip.
    /// Call this in steady state.
    pub(crate) fn load_wanted_tenures_at_tip(
        last_tip: Option<&BlockSnapshot>,
        tip: &BlockSnapshot,
        sortdb: &SortitionDB,
        loaded_so_far: &[WantedTenure],
    ) -> Result<Vec<WantedTenure>, NetError> {
        let tip_rc = sortdb
            .pox_constants
            .block_height_to_reward_cycle(sortdb.first_block_height, tip.block_height)
            .unwrap_or(0);

        let first_block_height = if let Some(highest_wanted_tenure) = loaded_so_far.last() {
            highest_wanted_tenure.burn_height + 1
        } else if let Some(last_tip) = last_tip.as_ref() {
            last_tip.block_height + 1
        } else {
            // careful -- need .saturating_sub(1) since this calculation puts the reward cycle start at
            // block height 1 mod reward cycle len, but we really want 0 mod reward cycle len.
            sortdb
                .pox_constants
                .reward_cycle_to_block_height(sortdb.first_block_height, tip_rc)
                .saturating_sub(1)
        };

        // be extra careful with last_block_height -- we not only account for the above, but also
        // we need to account for the fact that `load_wanted_tenures` does not load the sortition
        // of the last block height (but we want this!)
        let last_block_height = sortdb
            .pox_constants
            .reward_cycle_to_block_height(sortdb.first_block_height, tip_rc.saturating_add(1))
            .saturating_sub(1)
            .min(tip.block_height)
            .saturating_add(1);

        test_debug!(
            "Load tip sortitions between {} and {} (loaded_so_far = {})",
            first_block_height,
            last_block_height,
            loaded_so_far.len()
        );
        if last_block_height < first_block_height {
            return Ok(vec![]);
        }

        let ih = sortdb.index_handle(&tip.sortition_id);
        let wanted_tenures = Self::load_wanted_tenures(&ih, first_block_height, last_block_height)?;

        test_debug!(
            "Loaded tip sortitions between {} and {} (loaded_so_far = {}): {:?}",
            first_block_height,
            last_block_height,
            loaded_so_far.len(),
            &wanted_tenures
        );
        Ok(wanted_tenures)
    }

    /// Update the .processed state for each wanted tenure.
    /// Set it to true if any of the following are true:
    /// * we have processed the tenure already
    /// * we have downloaded and stored the full tenure
    pub(crate) fn inner_update_processed_wanted_tenures(
        nakamoto_start: u64,
        wanted_tenures: &mut [WantedTenure],
        chainstate: &StacksChainState,
    ) -> Result<(), NetError> {
        for wt in wanted_tenures.iter_mut() {
            test_debug!("update_processed_wanted_tenures: consider {:?}", &wt);
            if wt.processed {
                continue;
            }
            if wt.burn_height < nakamoto_start {
                // not our problem
                wt.processed = true;
                continue;
            }
            if NakamotoChainState::has_processed_nakamoto_tenure(
                chainstate.db(),
                &wt.tenure_id_consensus_hash,
            )? {
                test_debug!("Tenure {} is now processed", &wt.tenure_id_consensus_hash);
                wt.processed = true;
                continue;
            }
        }
        Ok(())
    }

    /// Update the .processed state for each wanted tenure
    pub(crate) fn update_processed_tenures(
        &mut self,
        chainstate: &StacksChainState,
    ) -> Result<(), NetError> {
        if let Some(prev_wanted_tenures) = self.prev_wanted_tenures.as_mut() {
            test_debug!("update_processed_wanted_tenures: update prev_tenures");
            Self::inner_update_processed_wanted_tenures(
                self.nakamoto_start_height,
                prev_wanted_tenures,
                chainstate,
            )?;
        }
        test_debug!("update_processed_wanted_tenures: update wanted_tenures");
        Self::inner_update_processed_wanted_tenures(
            self.nakamoto_start_height,
            &mut self.wanted_tenures,
            chainstate,
        )
    }

    /// Find all tenure-start blocks for a list of wanted tenures.
    pub(crate) fn load_tenure_start_blocks(
        wanted_tenures: &[WantedTenure],
        chainstate: &StacksChainState,
    ) -> Result<HashMap<StacksBlockId, NakamotoBlock>, NetError> {
        let mut tenure_start_blocks = HashMap::new();
        for wt in wanted_tenures {
            let Some(tenure_start_block) = chainstate
                .nakamoto_blocks_db()
                .get_nakamoto_tenure_start_block(&wt.tenure_id_consensus_hash)?
            else {
                continue;
            };
            tenure_start_blocks.insert(tenure_start_block.block_id(), tenure_start_block);
        }
        Ok(tenure_start_blocks)
    }

    /// Update our local tenure start block data
    fn update_tenure_start_blocks(
        &mut self,
        chainstate: &StacksChainState,
    ) -> Result<(), NetError> {
        let tenure_start_blocks = Self::load_tenure_start_blocks(&self.wanted_tenures, chainstate)?;
        self.tenure_start_blocks
            .extend(tenure_start_blocks.into_iter());
        Ok(())
    }

    /// Extended wanted tenures for the current reward cycle
    fn extend_wanted_tenures(
        &mut self,
        network: &PeerNetwork,
        sortdb: &SortitionDB,
        chainstate: &StacksChainState,
    ) -> Result<(), NetError> {
        let sort_tip = &network.burnchain_tip;
        let Some(invs) = network.inv_state_nakamoto.as_ref() else {
            // nothing to do
            test_debug!("No network inventories");
            return Err(NetError::PeerNotConnected);
        };

        let last_sort_height_opt = self.last_sort_tip.as_ref().map(|sn| sn.block_height);
        let last_sort_height = last_sort_height_opt.unwrap_or(sort_tip.block_height);
        let sort_rc = sortdb
            .pox_constants
            .block_height_to_reward_cycle(sortdb.first_block_height, last_sort_height)
            .expect("FATAL: burnchain tip is before system start");

        let mut new_wanted_tenures = Self::load_wanted_tenures_at_tip(
            self.last_sort_tip.as_ref(),
            sort_tip,
            sortdb,
            &self.wanted_tenures,
        )?;
        let new_tenure_start_blocks =
            Self::load_tenure_start_blocks(&new_wanted_tenures, chainstate)?;

        let can_advance_wanted_tenures =
            if let Some(prev_wanted_tenures) = self.prev_wanted_tenures.as_ref() {
                !Self::have_unprocessed_tenures(
                    &self.tenure_downloads.completed_tenures,
                    prev_wanted_tenures,
                    &self.tenure_block_ids,
                    &sortdb.pox_constants,
                    sortdb.first_block_height,
                    invs.inventories.values(),
                )
            } else {
                test_debug!("No prev_wanted_tenures yet");
                true
            };

        if can_advance_wanted_tenures && self.reward_cycle != sort_rc {
            let mut prev_wanted_tenures = vec![];
            let mut cur_wanted_tenures = vec![];
            let prev_wts = self.prev_wanted_tenures.take().unwrap_or(vec![]);
            let cur_wts = std::mem::replace(&mut self.wanted_tenures, vec![]);

            for wt in new_wanted_tenures
                .into_iter()
                .chain(prev_wts.into_iter())
                .chain(cur_wts.into_iter())
            {
                test_debug!("Consider wanted tenure: {:?}", &wt);
                let wt_rc = sortdb
                    .pox_constants
                    .block_height_to_reward_cycle(sortdb.first_block_height, wt.burn_height)
                    .expect("FATAL: height before system start");
                if wt_rc + 1 == sort_rc {
                    prev_wanted_tenures.push(wt);
                } else if wt_rc == sort_rc {
                    cur_wanted_tenures.push(wt);
                } else {
                    test_debug!("Drop wanted tenure: {:?}", &wt);
                }
            }

            prev_wanted_tenures.sort_by(|wt1, wt2| wt1.burn_height.cmp(&wt2.burn_height));
            cur_wanted_tenures.sort_by(|wt1, wt2| wt1.burn_height.cmp(&wt2.burn_height));

            test_debug!("prev_wanted_tenures is now {:?}", &prev_wanted_tenures);
            test_debug!("wanted_tenures is now {:?}", &cur_wanted_tenures);

            self.prev_wanted_tenures = if prev_wanted_tenures.len() > 0 {
                Some(prev_wanted_tenures)
            } else {
                None
            };
            self.wanted_tenures = cur_wanted_tenures;
            self.reward_cycle = sort_rc;
        } else {
            test_debug!(
                "Append {} wanted tenures: {:?}",
                new_wanted_tenures.len(),
                &new_wanted_tenures
            );
            self.wanted_tenures.append(&mut new_wanted_tenures);
            test_debug!("wanted_tenures is now {:?}", &self.wanted_tenures);
        }

        self.tenure_start_blocks
            .extend(new_tenure_start_blocks.into_iter());

        Ok(())
    }

    /// Initialize `self.wanted_tenures` and `self.prev_wanted_tenures` for the first time, if they
    /// do not exist.  At all times, `self.prev_wanted_tenures` ought to be initialized to the last
    /// full reward cycle's tenures, and `self.wanted_tenures` ought to be initialized to the
    /// ongoing reward cycle's tenures.
    pub(crate) fn initialize_wanted_tenures(
        &mut self,
        sort_tip: &BlockSnapshot,
        sortdb: &SortitionDB,
    ) -> Result<(), NetError> {
        if self
            .prev_wanted_tenures
            .as_ref()
            .map(|pwts| pwts.len())
            .unwrap_or(0)
            < usize::try_from(sortdb.pox_constants.reward_cycle_length)
                .expect("FATAL: usize cannot support reward cycle length")
        {
            // this is the first-ever pass, so load up the last full reward cycle
            let sort_rc = sortdb
                .pox_constants
                .block_height_to_reward_cycle(sortdb.first_block_height, sort_tip.block_height)
                .expect("FATAL: burnchain tip is before system start")
                .saturating_sub(1);

            let mut prev_wanted_tenures = vec![];
            Self::update_wanted_tenures_for_reward_cycle(
                sort_rc,
                sort_tip,
                sortdb,
                &mut prev_wanted_tenures,
            )?;

            test_debug!(
                "initial prev_wanted_tenures (rc {}): {:?}",
                sort_rc,
                &prev_wanted_tenures
            );
            self.prev_wanted_tenures = Some(prev_wanted_tenures);
        }
        if self.wanted_tenures.len() == 0 {
            // this is the first-ever pass, so load up the current reward cycle
            let sort_rc = sortdb
                .pox_constants
                .block_height_to_reward_cycle(sortdb.first_block_height, sort_tip.block_height)
                .expect("FATAL: burnchain tip is before system start");

            let mut wanted_tenures = vec![];
            Self::update_wanted_tenures_for_reward_cycle(
                sort_rc,
                sort_tip,
                sortdb,
                &mut wanted_tenures,
            )?;

            test_debug!(
                "initial wanted_tenures (rc {}): {:?}",
                sort_rc,
                &wanted_tenures
            );
            self.wanted_tenures = wanted_tenures;
            self.reward_cycle = sort_rc;
        }
        Ok(())
    }

    /// Determine if the set of `TenureStartEnd`s represents available but unfetched data.  Used to
    /// determine whether or not to update the set of wanted tenures -- we don't want to skip
    /// fetching wanted tenures if they're still available!
    pub(crate) fn have_unprocessed_tenures<'a>(
        completed_tenures: &HashSet<ConsensusHash>,
        prev_wanted_tenures: &[WantedTenure],
        tenure_block_ids: &HashMap<NeighborAddress, AvailableTenures>,
        pox_constants: &PoxConstants,
        first_burn_height: u64,
        inventory_iter: impl Iterator<Item = &'a NakamotoTenureInv>,
    ) -> bool {
        if prev_wanted_tenures.len() == 0 {
            return true;
        }

        // the anchor block for prev_wanted_tenures must not only be processed, but also we have to
        // have seen an inventory message from the subsequent reward cycle.  If we can see
        // inventory messages for the reward cycle after `prev_wanted_rc`, then the former will be
        // true
        let prev_wanted_rc = prev_wanted_tenures
            .first()
            .map(|wt| {
                pox_constants
                    .block_height_to_reward_cycle(first_burn_height, wt.burn_height)
                    .expect("FATAL: wanted tenure before system start")
            })
            .unwrap_or(u64::MAX);

        let cur_wanted_rc = prev_wanted_rc.saturating_add(1);

        let mut has_prev_inv = false;
        let mut has_cur_inv = false;
        for inv in inventory_iter {
            if inv.tenures_inv.get(&prev_wanted_rc).is_some() {
                has_prev_inv = true;
            }
            if inv.tenures_inv.get(&cur_wanted_rc).is_some() {
                has_cur_inv = true;
            }
        }

        if !has_prev_inv || !has_cur_inv {
            test_debug!("No peer has an inventory for either the previous ({}) or current ({}) wanted tenures", prev_wanted_rc, cur_wanted_rc);
            return true;
        }

        // the state machine updates `tenure_block_ids` _after_ `wanted_tenures`, so verify that
        // this isn't a stale `tenure_block_ids` by checking that it contains at least one block in
        // the prev_wanted_rc and at least one in the cur_wanted_rc
        let mut has_prev_rc_block = false;
        let mut has_cur_rc_block = false;
        for (_naddr, available) in tenure_block_ids.iter() {
            for (_ch, tenure_info) in available.iter() {
                if tenure_info.start_reward_cycle == prev_wanted_rc
                    || tenure_info.end_reward_cycle == prev_wanted_rc
                {
                    has_prev_rc_block = true;
                }
                if tenure_info.start_reward_cycle == cur_wanted_rc
                    || tenure_info.end_reward_cycle == cur_wanted_rc
                {
                    has_cur_rc_block = true;
                }
            }
        }

        if !has_prev_rc_block || !has_cur_rc_block {
            test_debug!(
                "tenure_block_ids stale: missing representation in reward cycles {} and {}",
                prev_wanted_rc,
                cur_wanted_rc
            );
            return true;
        }

        let mut ret = false;
        for (_naddr, available) in tenure_block_ids.iter() {
            for wt in prev_wanted_tenures.iter() {
                let Some(tenure_info) = available.get(&wt.tenure_id_consensus_hash) else {
                    continue;
                };
                if completed_tenures.contains(&tenure_info.tenure_id_consensus_hash) {
                    // this check is necessary because the check for .processed requires that a
                    // child tenure block has been processed, which isn't guaranteed at a reward
                    // cycle boundary
                    test_debug!("Tenure {:?} has been fully downloaded", &tenure_info);
                    continue;
                }
                if !tenure_info.processed {
                    test_debug!(
                        "Tenure {:?} is available from {} but not processed",
                        &tenure_info,
                        &_naddr
                    );
                    ret = true;
                }
            }
        }
        ret
    }

    /// Update the state machine's wanted tenures and processed tenures, if it's time to do so.
    /// This will only happen when the sortition DB has finished processing a reward cycle of
    /// tenures when in IBD mode, _OR_ when the sortition tip advances when in steady-state mode.
    ///
    /// In the first case, this function will load up the whole list of wanted
    /// tenures for this reward cycle, and proceed to download them.  This happens only on reward
    /// cycle boundaries.  The current list of wanted tenures will be saved as
    /// `self.prev_wanted_tenures` so that any tenures not yet downloaded from the ongoing reward
    /// cycle can be fetched.
    ///
    /// In the second case, this function will load up _new_
    pub(crate) fn update_wanted_tenures(
        &mut self,
        network: &PeerNetwork,
        sortdb: &SortitionDB,
        chainstate: &StacksChainState,
    ) -> Result<(), NetError> {
        let sort_tip = &network.burnchain_tip;
        let Some(invs) = network.inv_state_nakamoto.as_ref() else {
            // nothing to do
            test_debug!("No network inventories");
            return Err(NetError::PeerNotConnected);
        };

        self.initialize_wanted_tenures(sort_tip, sortdb)?;
        let last_sort_height_opt = self.last_sort_tip.as_ref().map(|sn| sn.block_height);
        let last_sort_height = last_sort_height_opt.unwrap_or(sort_tip.block_height);
        let sort_rc = sortdb
            .pox_constants
            .block_height_to_reward_cycle(sortdb.first_block_height, last_sort_height)
            .expect("FATAL: burnchain tip is before system start");

        let next_sort_rc = if last_sort_height == sort_tip.block_height {
            sortdb
                .pox_constants
                .block_height_to_reward_cycle(
                    sortdb.first_block_height,
                    sort_tip.block_height.saturating_add(1),
                )
                .expect("FATAL: burnchain tip is before system start")
        } else {
            sortdb
                .pox_constants
                .block_height_to_reward_cycle(sortdb.first_block_height, sort_tip.block_height)
                .expect("FATAL: burnchain tip is before system start")
        };

        test_debug!(
            "last_sort_height = {}, sort_rc = {}, next_sort_rc = {}, self.reward_cycle = {}, sort_tip.block_height = {}",
            last_sort_height,
            sort_rc,
            next_sort_rc,
            self.reward_cycle,
            sort_tip.block_height,
        );

        if sort_rc == next_sort_rc {
            test_debug!("Extend wanted tenures since no sort_rc change and we have tenure data");
            return self.extend_wanted_tenures(network, sortdb, chainstate);
        }

        let can_advance_wanted_tenures =
            if let Some(prev_wanted_tenures) = self.prev_wanted_tenures.as_ref() {
                !Self::have_unprocessed_tenures(
                    &self.tenure_downloads.completed_tenures,
                    prev_wanted_tenures,
                    &self.tenure_block_ids,
                    &sortdb.pox_constants,
                    sortdb.first_block_height,
                    invs.inventories.values(),
                )
            } else {
                test_debug!("No prev_wanted_tenures yet");
                true
            };
        if !can_advance_wanted_tenures {
            return Ok(());
        }

        // crossed reward cycle boundary
        let mut new_wanted_tenures = vec![];
        Self::update_wanted_tenures_for_reward_cycle(
            sort_rc + 1,
            sort_tip,
            sortdb,
            &mut new_wanted_tenures,
        )?;

        let mut new_prev_wanted_tenures = vec![];
        Self::update_wanted_tenures_for_reward_cycle(
            sort_rc,
            sort_tip,
            sortdb,
            &mut new_prev_wanted_tenures,
        )?;

        test_debug!("new_wanted_tenures is now {:?}", &new_wanted_tenures);
        test_debug!(
            "new_prev_wanted_tenures is now {:?}",
            &new_prev_wanted_tenures
        );

        self.prev_wanted_tenures = if new_prev_wanted_tenures.len() > 0 {
            Some(new_prev_wanted_tenures)
        } else {
            None
        };
        self.wanted_tenures = new_wanted_tenures;
        self.reward_cycle = sort_rc;

        self.update_tenure_start_blocks(chainstate)?;
        Ok(())
    }

    /// Given a set of inventory bit vectors for the current reward cycle, find out which neighbors
    /// can serve each tenure (identified by the tenure ID consensus hash).
    /// Every tenure ID consensus hash in `wanted_tenures` will be mapped to the returned hash
    /// table, but the list of addresses may be empty.
    pub(crate) fn find_available_tenures<'a>(
        reward_cycle: u64,
        wanted_tenures: &[WantedTenure],
        mut inventory_iter: impl Iterator<Item = (&'a NeighborAddress, &'a NakamotoTenureInv)>,
    ) -> HashMap<ConsensusHash, Vec<NeighborAddress>> {
        let mut available: HashMap<ConsensusHash, Vec<NeighborAddress>> = HashMap::new();
        for wt in wanted_tenures.iter() {
            available.insert(wt.tenure_id_consensus_hash.clone(), vec![]);
        }

        while let Some((naddr, inv)) = inventory_iter.next() {
            let Some(rc_inv) = inv.tenures_inv.get(&reward_cycle) else {
                // this peer has no inventory data for this reward cycle
                test_debug!(
                    "Peer {} has no inventory for reward cycle {}",
                    naddr,
                    reward_cycle
                );
                continue;
            };
            for (i, wt) in wanted_tenures.iter().enumerate() {
                if wt.processed {
                    continue;
                }

                let (ch, ibh) = (&wt.tenure_id_consensus_hash, &wt.winning_block_id);
                if ibh == &StacksBlockId([0x00; 32]) {
                    continue;
                }

                let bit = u16::try_from(i).expect("FATAL: more sortitions than u16::MAX");
                if !rc_inv.get(bit).unwrap_or(false) {
                    // this neighbor does not have this tenure
                    test_debug!(
                        "Peer {} does not have sortition #{} in reward cycle {} (wt {:?})",
                        naddr,
                        bit,
                        reward_cycle,
                        &wt
                    );
                    continue;
                }

                if let Some(neighbor_list) = available.get_mut(ch) {
                    neighbor_list.push(naddr.clone());
                } else {
                    available.insert(ch.clone(), vec![naddr.clone()]);
                }
            }
        }
        available
    }

    /// Find each peer's mapping between tenure ID consensus hashes for the tenures it claims to
    /// have, and its tenure start block ID
    pub(crate) fn find_tenure_block_ids<'a>(
        rc: u64,
        wanted_tenures: &[WantedTenure],
        next_wanted_tenures: Option<&[WantedTenure]>,
        pox_constants: &PoxConstants,
        first_burn_height: u64,
        mut inventory_iter: impl Iterator<Item = (&'a NeighborAddress, &'a NakamotoTenureInv)>,
    ) -> HashMap<NeighborAddress, AvailableTenures> {
        let mut tenure_block_ids = HashMap::new();
        while let Some((naddr, tenure_inv)) = inventory_iter.next() {
            let Some(peer_tenure_block_ids) = TenureStartEnd::from_inventory(
                rc,
                wanted_tenures,
                next_wanted_tenures,
                pox_constants,
                first_burn_height,
                tenure_inv,
            ) else {
                // this peer doesn't know about this reward cycle
                continue;
            };
            tenure_block_ids.insert(naddr.clone(), peer_tenure_block_ids);
        }
        tenure_block_ids
    }

    /// Produce a download schedule for IBD mode.  Tenures will be downloaded in sortition order.
    /// The first item will be fetched first.
    pub(crate) fn make_ibd_download_schedule(
        nakamoto_start: u64,
        wanted_tenures: &[WantedTenure],
        available: &HashMap<ConsensusHash, Vec<NeighborAddress>>,
    ) -> VecDeque<ConsensusHash> {
        let mut schedule = VecDeque::new();
        for wt in wanted_tenures.iter() {
            if wt.processed {
                continue;
            }
            if wt.burn_height < nakamoto_start {
                continue;
            }
            if !available.contains_key(&wt.tenure_id_consensus_hash) {
                continue;
            }
            schedule.push_back(wt.tenure_id_consensus_hash.clone());
        }
        schedule
    }

    /// Produce a download schedule for steady-state mode.  Tenures will be downloaded in
    /// rarest-first order.
    /// The first item will be fetched first.
    pub(crate) fn make_rarest_first_download_schedule(
        nakamoto_start: u64,
        wanted_tenures: &[WantedTenure],
        available: &HashMap<ConsensusHash, Vec<NeighborAddress>>,
        // TODO: unconfirmed tenure downloader
    ) -> VecDeque<ConsensusHash> {
        let mut schedule = Vec::with_capacity(available.len());
        for wt in wanted_tenures.iter() {
            if wt.processed {
                continue;
            }
            if wt.burn_height < nakamoto_start {
                continue;
            }
            let Some(neighbors) = available.get(&wt.tenure_id_consensus_hash) else {
                continue;
            };
            schedule.push((neighbors.len(), wt.tenure_id_consensus_hash.clone()));
        }

        // order by fewest neighbors first
        schedule.sort_by(|a, b| a.0.cmp(&b.0));
        schedule.into_iter().map(|(_count, ch)| ch).collect()
    }

    /// How many neighbors can we contact still?
    fn count_available_tenure_neighbors(
        available: &HashMap<ConsensusHash, Vec<NeighborAddress>>,
    ) -> usize {
        available
            .iter()
            .fold(0, |count, (_ch, naddrs)| count.saturating_add(naddrs.len()))
    }

    /// Update our available tenure set and download schedule.
    /// Call after Self::update_wanted_tenures()
    fn update_available_tenures(
        &mut self,
        inventories: &HashMap<NeighborAddress, NakamotoTenureInv>,
        pox_constants: &PoxConstants,
        first_burn_height: u64,
        ibd: bool,
    ) {
        if self.tenure_download_schedule.is_empty() {
            // try again
            self.available_tenures.clear();
            self.tenure_block_ids.clear();
        }
        if Self::count_available_tenure_neighbors(&self.available_tenures) > 0 {
            // still have requests to try, so don't bother computing a new set of available tenures
            test_debug!("Still have requests to try");
            return;
        }
        if self.wanted_tenures.len() == 0 {
            // nothing to do
            return;
        }
        if inventories.len() == 0 {
            // nothing to do
            test_debug!("No inventories available");
            return;
        }

        // get available tenures for both the current and previous reward cycles
        let prev_available = self
            .prev_wanted_tenures
            .as_ref()
            .map(|prev_wanted_tenures| {
                test_debug!(
                    "Load availability for prev_wanted_tenures ({}) at rc {}",
                    prev_wanted_tenures.len(),
                    self.reward_cycle.saturating_sub(1)
                );
                Self::find_available_tenures(
                    self.reward_cycle.saturating_sub(1),
                    prev_wanted_tenures,
                    inventories.iter(),
                )
            })
            .unwrap_or(HashMap::new());

        let mut available = Self::find_available_tenures(
            self.reward_cycle,
            &self.wanted_tenures,
            inventories.iter(),
        );
        available.extend(prev_available.into_iter());

        let prev_tenure_block_ids = self.prev_wanted_tenures
            .as_ref()
            .map(|prev_wanted_tenures| {
                // have both self.prev_wanted_tenures and self.wanted_tenures
                test_debug!("Load tenure block IDs for prev_wanted_tenures ({}) and wanted_tenures ({}) at rc {}", prev_wanted_tenures.len(), self.wanted_tenures.len(), self.reward_cycle.saturating_sub(1));
                Self::find_tenure_block_ids(
                    self.reward_cycle.saturating_sub(1),
                    prev_wanted_tenures,
                    Some(&self.wanted_tenures),
                    pox_constants,
                    first_burn_height,
                    inventories.iter(),
                )
            })
            .unwrap_or(HashMap::new());

        let mut tenure_block_ids = {
            test_debug!(
                "Load tenure block IDs for wanted_tenures ({}) at rc {}",
                self.wanted_tenures.len(),
                self.reward_cycle
            );
            Self::find_tenure_block_ids(
                self.reward_cycle,
                &self.wanted_tenures,
                None,
                pox_constants,
                first_burn_height,
                inventories.iter(),
            )
        };

        // merge tenure block IDs
        for (naddr, prev_available) in prev_tenure_block_ids.into_iter() {
            if let Some(available) = tenure_block_ids.get_mut(&naddr) {
                available.extend(prev_available.into_iter());
            } else {
                tenure_block_ids.insert(naddr, prev_available);
            }
        }

        // create download schedules for unprocessed blocks
        let schedule = if ibd {
            let mut prev_schedule = self
                .prev_wanted_tenures
                .as_ref()
                .map(|prev_wanted_tenures| {
                    Self::make_ibd_download_schedule(
                        self.nakamoto_start_height,
                        prev_wanted_tenures,
                        &available,
                    )
                })
                .unwrap_or(VecDeque::new());

            let schedule = Self::make_ibd_download_schedule(
                self.nakamoto_start_height,
                &self.wanted_tenures,
                &available,
            );

            prev_schedule.extend(schedule.into_iter());
            prev_schedule
        } else {
            let mut prev_schedule = self
                .prev_wanted_tenures
                .as_ref()
                .map(|prev_wanted_tenures| {
                    Self::make_rarest_first_download_schedule(
                        self.nakamoto_start_height,
                        prev_wanted_tenures,
                        &available,
                    )
                })
                .unwrap_or(VecDeque::new());

            let schedule = Self::make_rarest_first_download_schedule(
                self.nakamoto_start_height,
                &self.wanted_tenures,
                &available,
            );

            prev_schedule.extend(schedule.into_iter());
            prev_schedule
        };

        test_debug!("new schedule: {:?}", schedule);
        test_debug!("new available: {:?}", &available);
        test_debug!("new tenure_block_ids: {:?}", &tenure_block_ids);

        self.tenure_download_schedule = schedule;
        self.tenure_block_ids = tenure_block_ids;
        self.available_tenures = available;
    }

    /// Update our tenure download state machines
    fn update_tenure_downloaders(
        &mut self,
        count: usize,
        agg_public_keys: &BTreeMap<u64, Option<Point>>,
    ) {
        self.tenure_downloads.make_tenure_downloaders(
            &mut self.tenure_download_schedule,
            &mut self.available_tenures,
            &mut self.tenure_block_ids,
            count,
            agg_public_keys,
        )
    }

    /// Determine whether or not we can start downloading the highest complete tenure and the
    /// unconfirmed tenure.  Only do this if (1) the sortition DB is at the burnchain tip and (2)
    /// all of our wanted tenures are marked as either downloaded or complete.
    ///
    /// To fully determine if it's appropriate to download unconfirmed tenures, the caller should
    /// additionally ensure that there are no in-flight confirmed tenure downloads.
    pub(crate) fn need_unconfirmed_tenures<'a>(
        burnchain_height: u64,
        sort_tip: &BlockSnapshot,
        completed_tenures: &HashSet<ConsensusHash>,
        wanted_tenures: &[WantedTenure],
        prev_wanted_tenures: &[WantedTenure],
        tenure_block_ids: &HashMap<NeighborAddress, AvailableTenures>,
        pox_constants: &PoxConstants,
        first_burn_height: u64,
        inventory_iter: impl Iterator<Item = &'a NakamotoTenureInv>,
        blocks_db: NakamotoStagingBlocksConnRef,
    ) -> bool {
        if sort_tip.block_height < burnchain_height {
            test_debug!(
                "sort_tip {} < burn tip {}",
                sort_tip.block_height,
                burnchain_height
            );
            return false;
        }

        if wanted_tenures.len() == 0 {
            test_debug!("No wanted tenures");
            return false;
        }

        if prev_wanted_tenures.len() == 0 {
            test_debug!("No prev wanted tenures");
            return false;
        }

        // there are still confirmed tenures we have to go and get
        if Self::have_unprocessed_tenures(
            completed_tenures,
            prev_wanted_tenures,
            tenure_block_ids,
            pox_constants,
            first_burn_height,
            inventory_iter,
        ) {
            test_debug!("Still have unprocessed tenures, so we don't need unconfirmed tenures");
            return false;
        }

        // see if we need any tenures still
        let mut need_tenure = false;
        for (_naddr, available) in tenure_block_ids.iter() {
            for wt in wanted_tenures.iter() {
                if !available.contains_key(&wt.tenure_id_consensus_hash) {
                    continue;
                }
                if completed_tenures.contains(&wt.tenure_id_consensus_hash) {
                    continue;
                }
                if !wt.processed {
                    test_debug!(
                        "Still need tenure {} from {}",
                        &wt.tenure_id_consensus_hash,
                        _naddr
                    );
                    need_tenure = true;
                    break;
                }
            }
        }

        if need_tenure {
            return false;
        }

        // there are still tenures that have to be processed
        if blocks_db
            .has_any_unprocessed_nakamoto_block()
            .map_err(|e| {
                warn!(
                    "Failed to determine if there are unprocessed Nakamoto blocks: {:?}",
                    &e
                );
                e
            })
            .unwrap_or(true)
        {
            test_debug!("Still have stored but unprocessed Nakamoto blocks");
            return false;
        }

        true
    }

    /// Select neighbors to query for unconfirmed tenures, given this node's view of the burnchain
    /// and an iterator over the set of ongoing p2p conversations.
    /// Only select neighbors that has the same burnchain view as us, and have authenticated to us
    /// and are outbound from us (meaning, they're not NAT'ed relative to us).
    pub(crate) fn make_unconfirmed_tenure_download_schedule<'a>(
        chain_view: &BurnchainView,
        peers_iter: impl Iterator<Item = (&'a usize, &'a ConversationP2P)>,
    ) -> VecDeque<NeighborAddress> {
        let mut schedule = VecDeque::new();
        for (_, convo) in peers_iter {
            if chain_view.burn_block_hash != convo.burnchain_tip_burn_header_hash {
                continue;
            }
            if chain_view.burn_block_height != convo.burnchain_tip_height {
                continue;
            }
            if !convo.is_authenticated() {
                continue;
            }
            if !convo.is_outbound() {
                continue;
            }
            schedule.push_back(convo.to_neighbor_address());
        }
        schedule
    }

    /// Create up to `count` unconfirmed tenure downloaders.  Add them to `downloaders`, and remove
    /// the remote peer's address from `schedule`.
    ///
    /// The caller will need to ensure that no request to the ongoing unconfirmed tenure
    /// downloaders gets created, lest it replace the unconfirmed tenure request.
    pub(crate) fn make_unconfirmed_tenure_downloaders(
        schedule: &mut VecDeque<NeighborAddress>,
        count: usize,
        downloaders: &mut HashMap<NeighborAddress, NakamotoUnconfirmedTenureDownloader>,
        highest_processed_block_id: Option<StacksBlockId>,
    ) {
        while downloaders.len() < count {
            let Some(naddr) = schedule.front() else {
                break;
            };
            if downloaders.contains_key(naddr) {
                continue;
            }
            let unconfirmed_tenure_download = NakamotoUnconfirmedTenureDownloader::new(
                naddr.clone(),
                highest_processed_block_id.clone(),
            );

            test_debug!("Request unconfirmed tenure state from neighbor {}", &naddr);
            downloaders.insert(naddr.clone(), unconfirmed_tenure_download);
            schedule.pop_front();
        }
    }

    /// Update our unconfirmed tenure download state machines
    fn update_unconfirmed_tenure_downloaders(
        &mut self,
        count: usize,
        highest_processed_block_id: Option<StacksBlockId>,
    ) {
        Self::make_unconfirmed_tenure_downloaders(
            &mut self.unconfirmed_tenure_download_schedule,
            count,
            &mut self.unconfirmed_tenure_downloads,
            highest_processed_block_id,
        );
    }

    /// Run unconfirmed tenure downloads.
    /// As the local node processes blocks, update each downloader's view of the highest-processed
    /// block so it can cancel itself early if it finds that we've already got the blocks.
    /// Returns the map from neighbors to the unconfirmed blocks they serve, as well as a map from
    /// neighbors to the instantiated confirmed tenure downloaders for their highest completed
    /// tenures (this information cannot be determined from sortition history and block inventories
    /// alone, since we need to know the tenure-start block from the ongoing tenure).
    ///
    /// This method guarantees that the highest confirmed tenure downloaders instantiated here can
    /// be safely run without clobbering ongoing conversations with other neighbors, _provided
    /// that_ the download state machine is currently concerned with running unconfirmed tenure
    /// downloaders (i.e. it's not in IBD).
    pub(crate) fn run_unconfirmed_downloaders(
        downloaders: &mut HashMap<NeighborAddress, NakamotoUnconfirmedTenureDownloader>,
        network: &mut PeerNetwork,
        neighbor_rpc: &mut NeighborRPC,
        sortdb: &SortitionDB,
        sort_tip: &BlockSnapshot,
        chainstate: &StacksChainState,
        highest_complete_tenure: &WantedTenure,
        unconfirmed_tenure: &WantedTenure,
    ) -> (
        HashMap<NeighborAddress, Vec<NakamotoBlock>>,
        HashMap<NeighborAddress, NakamotoTenureDownloader>,
    ) {
        let addrs: Vec<_> = downloaders.keys().map(|addr| addr.clone()).collect();
        let mut finished = vec![];
        let mut unconfirmed_blocks = HashMap::new();
        let mut highest_completed_tenure_downloaders = HashMap::new();

        // find the highest-processed block, and update all ongoing state-machines.
        // Then, as faster state-machines linked to more up-to-date peers download newer blocks,
        // other state-machines will automatically terminate once they reach the highest block this
        // peer has now processed.
        let highest_processed_block_id =
            StacksBlockId::new(&network.stacks_tip.0, &network.stacks_tip.1);
        let highest_processed_block_height = network.stacks_tip.2;

        for (_, downloader) in downloaders.iter_mut() {
            downloader.set_highest_processed_block(
                highest_processed_block_id.clone(),
                highest_processed_block_height,
            );
        }

        // send requests
        for (naddr, downloader) in downloaders.iter_mut() {
            if downloader.is_done() {
                finished.push(naddr.clone());
                continue;
            }
            if neighbor_rpc.has_inflight(&naddr) {
                continue;
            }

            test_debug!(
                "Send request to {} for tenure {:?} (state {})",
                &naddr,
                &downloader.unconfirmed_tenure_id(),
                &downloader.state
            );
            let Ok(sent) = downloader.send_next_download_request(network, neighbor_rpc) else {
                neighbor_rpc.add_dead(network, naddr);
                continue;
            };
            if !sent {
                finished.push(naddr.clone());
                continue;
            }
        }

        // clear dead, broken, and done
        for naddr in addrs.iter() {
            if neighbor_rpc.is_dead_or_broken(network, naddr) {
                downloaders.remove(naddr);
            }
        }
        for done_naddr in finished.iter() {
            downloaders.remove(done_naddr);
        }
        finished.clear();

        // handle responses
        for (naddr, response) in neighbor_rpc.collect_replies(network) {
            let Some(downloader) = downloaders.get_mut(&naddr) else {
                test_debug!("Got rogue response from {}", &naddr);
                continue;
            };

            test_debug!("Got response from {}", &naddr);
            let Ok(blocks_opt) = downloader.handle_next_download_response(
                response,
                sortdb,
                sort_tip,
                chainstate,
                &network.aggregate_public_keys,
            ) else {
                neighbor_rpc.add_dead(network, &naddr);
                continue;
            };

            let Some(blocks) = blocks_opt else {
                continue;
            };

            if let Some(highest_complete_tenure_downloader) = downloader
                .make_highest_complete_tenure_downloader(
                    highest_complete_tenure,
                    unconfirmed_tenure,
                )
                .map_err(|e| {
                    warn!(
                        "Failed to make highest complete tenure downloader for {:?}: {:?}",
                        &downloader.unconfirmed_tenure_id(),
                        &e
                    );
                    e
                })
                .ok()
            {
                // don't start this unless the downloader is actually done (this should always be
                // the case, but don't tempt fate with an assert!)
                if downloader.is_done() {
                    highest_completed_tenure_downloaders
                        .insert(naddr.clone(), highest_complete_tenure_downloader);
                }
            }

            unconfirmed_blocks.insert(naddr.clone(), blocks);
            if downloader.is_done() {
                finished.push(naddr);
                continue;
            }
        }

        // clear dead, broken, and done
        for naddr in addrs.iter() {
            if neighbor_rpc.is_dead_or_broken(network, naddr) {
                downloaders.remove(naddr);
            }
        }
        for done_naddr in finished.iter() {
            downloaders.remove(done_naddr);
        }

        (unconfirmed_blocks, highest_completed_tenure_downloaders)
    }

    /// Run and process all confirmed tenure downloaders
    fn download_confirmed_tenures(
        &mut self,
        network: &mut PeerNetwork,
        max_count: usize,
    ) -> HashMap<ConsensusHash, Vec<NakamotoBlock>> {
        // queue up more downloaders
        self.update_tenure_downloaders(max_count, &network.aggregate_public_keys);

        // run all downloaders
        let new_blocks = self.tenure_downloads.run(network, &mut self.neighbor_rpc);

        // give blocked downloaders their tenure-end blocks from other downloaders that have
        // obtained their tenure-start blocks
        let new_tenure_starts = self.tenure_downloads.find_new_tenure_start_blocks();
        self.tenure_start_blocks
            .extend(new_tenure_starts.into_iter());

        let dead = self
            .tenure_downloads
            .handle_tenure_end_blocks(&self.tenure_start_blocks);

        // bookkeeping
        for naddr in dead.into_iter() {
            self.neighbor_rpc.add_dead(network, &naddr);
        }

        new_blocks
    }

    /// Run and process all unconfirmed tenure downloads, and highest-confirmed tenure downloads
    fn download_unconfirmed_tenures(
        &mut self,
        network: &mut PeerNetwork,
        sortdb: &SortitionDB,
        chainstate: &StacksChainState,
        highest_processed_block_id: Option<StacksBlockId>,
    ) -> HashMap<ConsensusHash, Vec<NakamotoBlock>> {
        // queue up more downloaders
        self.update_unconfirmed_tenure_downloaders(
            usize::try_from(network.get_connection_opts().max_inflight_blocks)
                .expect("FATAL: max_inflight_blocks exceeds usize::MAX"),
            highest_processed_block_id,
        );

        // run all unconfirmed downloaders, and start confirmed downloaders for the
        // highest-confirmed tenure
        let burnchain_tip = network.burnchain_tip.clone();
        let Some(unconfirmed_tenure) = self
            .wanted_tenures
            .last()
            .map(|wt| Some(wt.clone()))
            .unwrap_or_else(|| {
                // unconfirmed tenure is the last tenure in prev_wanted_tenures if
                // wanted_tenures.len() is 0
                let Some(prev_wanted_tenures) = self.prev_wanted_tenures.as_ref() else {
                    return None;
                };
                let Some(wt) = prev_wanted_tenures.last() else {
                    return None;
                };
                Some(wt.clone())
            })
        else {
            // not initialized yet (technically unrachable)
            return HashMap::new();
        };

        let highest_wanted_tenure = if self.wanted_tenures.len() == 0 {
            // highest complete wanted tenure is the second-to-last tenure in prev_wanted_tenures
            let Some(prev_wanted_tenures) = self.prev_wanted_tenures.as_ref() else {
                // not initialized yet (technically unrachable)
                return HashMap::new();
            };
            if prev_wanted_tenures.len() < 2 {
                return HashMap::new();
            };
            let Some(wt) = prev_wanted_tenures.get(prev_wanted_tenures.len().saturating_sub(2))
            else {
                return HashMap::new();
            };
            wt.clone()
        } else if self.wanted_tenures.len() == 1 {
            // highest complete tenure is the last tenure in prev_wanted_tenures
            let Some(prev_wanted_tenures) = self.prev_wanted_tenures.as_ref() else {
                return HashMap::new();
            };
            let Some(wt) = prev_wanted_tenures.last() else {
                return HashMap::new();
            };
            wt.clone()
        } else {
            // highest complete tenure is the second-to-last tenure in wanted_tenures
            let Some(wt) = self
                .wanted_tenures
                .get(self.wanted_tenures.len().saturating_sub(2))
            else {
                return HashMap::new();
            };
            wt.clone()
        };

        let new_confirmed_blocks = if self.tenure_downloads.inflight() > 0 {
            self.download_confirmed_tenures(network, 0)
        } else {
            HashMap::new()
        };

        let (new_unconfirmed_blocks, new_highest_confirmed_downloaders) =
            if self.tenure_downloads.inflight() > 0 {
                (HashMap::new(), HashMap::new())
            } else {
                Self::run_unconfirmed_downloaders(
                    &mut self.unconfirmed_tenure_downloads,
                    network,
                    &mut self.neighbor_rpc,
                    sortdb,
                    &burnchain_tip,
                    chainstate,
                    &highest_wanted_tenure,
                    &unconfirmed_tenure,
                )
            };

        // run downloaders for the highest-confirmed tenure
        self.tenure_downloads
            .add_downloaders(new_highest_confirmed_downloaders.into_iter());

        // coalesce blocks -- maps consensus hash to map of block id to block
        let mut coalesced_blocks: HashMap<ConsensusHash, HashMap<StacksBlockId, NakamotoBlock>> =
            HashMap::new();
        for blocks in new_unconfirmed_blocks
            .into_values()
            .chain(new_confirmed_blocks.into_values())
        {
            for block in blocks.into_iter() {
                let block_id = block.header.block_id();
                if let Some(block_map) = coalesced_blocks.get_mut(&block.header.consensus_hash) {
                    block_map.insert(block_id, block);
                } else {
                    let mut block_map = HashMap::new();
                    let ch = block.header.consensus_hash.clone();
                    block_map.insert(block_id, block);
                    coalesced_blocks.insert(ch, block_map);
                }
            }
        }

        coalesced_blocks
            .into_iter()
            .map(|(consensus_hash, block_map)| {
                let mut block_list: Vec<_> =
                    block_map.into_iter().map(|(_, block)| block).collect();
                block_list.sort_by(|blk_1, blk_2| {
                    blk_1.header.chain_length.cmp(&blk_2.header.chain_length)
                });
                (consensus_hash, block_list)
            })
            .collect()
    }

    /// Run all downloads, and transition the downloader in-between `confirmed` and `unconfirmed`
    /// modes as needed
    fn run_downloads(
        &mut self,
        burnchain_height: u64,
        network: &mut PeerNetwork,
        sortdb: &SortitionDB,
        chainstate: &StacksChainState,
        ibd: bool,
    ) -> HashMap<ConsensusHash, Vec<NakamotoBlock>> {
        debug!("NakamotoDownloadStateMachine in state {}", &self.state);
        let Some(invs) = network.inv_state_nakamoto.as_ref() else {
            // nothing to do
            test_debug!("No network inventories");
            return HashMap::new();
        };
        test_debug!(
            "run_downloads: burnchain_height={}, network.burnchain_tip.block_height={}",
            burnchain_height,
            network.burnchain_tip.block_height
        );
        self.update_available_tenures(
            &invs.inventories,
            &sortdb.pox_constants,
            sortdb.first_block_height,
            ibd,
        );

        match self.state {
            NakamotoDownloadState::Confirmed => {
                let new_blocks = self.download_confirmed_tenures(
                    network,
                    usize::try_from(network.get_connection_opts().max_inflight_blocks)
                        .expect("FATAL: max_inflight_blocks exceeds usize::MAX"),
                );

                // keep borrow-checker happy by instantiang this ref again, now that `network` is
                // no longer mutably borrowed.
                let Some(invs) = network.inv_state_nakamoto.as_ref() else {
                    // nothing to do
                    test_debug!("No network inventories");
                    return HashMap::new();
                };

                if self.tenure_downloads.is_empty()
                    && Self::need_unconfirmed_tenures(
                        burnchain_height,
                        &network.burnchain_tip,
                        &self.tenure_downloads.completed_tenures,
                        &self.wanted_tenures,
                        self.prev_wanted_tenures.as_ref().unwrap_or(&vec![]),
                        &self.tenure_block_ids,
                        &sortdb.pox_constants,
                        sortdb.first_block_height,
                        invs.inventories.values(),
                        chainstate.nakamoto_blocks_db(),
                    )
                {
                    debug!(
                        "Transition from {} to {}",
                        &self.state,
                        NakamotoDownloadState::Unconfirmed
                    );

                    self.unconfirmed_tenure_download_schedule =
                        Self::make_unconfirmed_tenure_download_schedule(
                            &network.chain_view,
                            network.iter_peer_convos(),
                        );
                    self.state = NakamotoDownloadState::Unconfirmed;
                }

                return new_blocks;
            }
            NakamotoDownloadState::Unconfirmed => {
                let highest_processed_block_id =
                    StacksBlockId::new(&network.stacks_tip.0, &network.stacks_tip.1);

                let new_blocks = self.download_unconfirmed_tenures(
                    network,
                    sortdb,
                    chainstate,
                    Some(highest_processed_block_id),
                );

                // keep borrow-checker happy by instantiang this ref again, now that `network` is
                // no longer mutably borrowed.
                let Some(invs) = network.inv_state_nakamoto.as_ref() else {
                    // nothing to do
                    test_debug!("No network inventories");
                    return HashMap::new();
                };

                if self.tenure_downloads.is_empty()
                    && self.unconfirmed_tenure_downloads.is_empty()
                    && self.unconfirmed_tenure_download_schedule.is_empty()
                {
                    if Self::need_unconfirmed_tenures(
                        burnchain_height,
                        &network.burnchain_tip,
                        &self.tenure_downloads.completed_tenures,
                        &self.wanted_tenures,
                        self.prev_wanted_tenures.as_ref().unwrap_or(&vec![]),
                        &self.tenure_block_ids,
                        &sortdb.pox_constants,
                        sortdb.first_block_height,
                        invs.inventories.values(),
                        chainstate.nakamoto_blocks_db(),
                    ) {
                        // do this again
                        self.unconfirmed_tenure_download_schedule =
                            Self::make_unconfirmed_tenure_download_schedule(
                                &network.chain_view,
                                network.iter_peer_convos(),
                            );
                        debug!(
                            "Transition from {} to {}",
                            &self.state,
                            NakamotoDownloadState::Unconfirmed
                        );
                        self.state = NakamotoDownloadState::Unconfirmed;
                    } else {
                        debug!(
                            "Transition from {} to {}",
                            &self.state,
                            NakamotoDownloadState::Confirmed
                        );
                        self.state = NakamotoDownloadState::Confirmed;
                    }
                }

                return new_blocks;
            }
        }
    }

    /// Go and get tenures. Returns list of blocks per tenure, identified by consensus hash.
    /// The blocks will be sorted by height, but may not be contiguous.
    pub fn run(
        &mut self,
        burnchain_tip: u64,
        network: &mut PeerNetwork,
        sortdb: &SortitionDB,
        chainstate: &StacksChainState,
        ibd: bool,
    ) -> Result<HashMap<ConsensusHash, Vec<NakamotoBlock>>, NetError> {
        self.update_wanted_tenures(&network, sortdb, chainstate)?;
        self.update_processed_tenures(chainstate)?;
        let new_blocks = self.run_downloads(burnchain_tip, network, sortdb, chainstate, ibd);
        self.last_sort_tip = Some(network.burnchain_tip.clone());
        Ok(new_blocks)
    }
}

impl PeerNetwork {
    /// Set up the Nakamoto block downloader
    pub fn init_nakamoto_block_downloader(&mut self) {
        if self.block_downloader_nakamoto.is_some() {
            return;
        }
        let epoch = self.get_epoch_by_epoch_id(StacksEpochId::Epoch30);
        let downloader = NakamotoDownloadStateMachine::new(epoch.start_height);
        self.block_downloader_nakamoto = Some(downloader);
    }

    /// Drive the block download state machine
    pub fn sync_blocks_nakamoto(
        &mut self,
        burnchain_tip: u64,
        sortdb: &SortitionDB,
        chainstate: &StacksChainState,
        ibd: bool,
    ) -> Result<HashMap<ConsensusHash, Vec<NakamotoBlock>>, NetError> {
        if self.block_downloader_nakamoto.is_none() {
            self.init_nakamoto_block_downloader();
        }
        let Some(mut block_downloader) = self.block_downloader_nakamoto.take() else {
            return Ok(HashMap::new());
        };

        let new_blocks_res = block_downloader.run(burnchain_tip, self, sortdb, chainstate, ibd);
        self.block_downloader_nakamoto = Some(block_downloader);

        new_blocks_res
    }

    /// Perform block sync.
    /// Drive the state machine, and clear out any dead and banned neighbors
    pub fn do_network_block_sync_nakamoto(
        &mut self,
        burnchain_tip: u64,
        sortdb: &SortitionDB,
        chainstate: &StacksChainState,
        ibd: bool,
    ) -> Result<HashMap<ConsensusHash, Vec<NakamotoBlock>>, NetError> {
        let res = self.sync_blocks_nakamoto(burnchain_tip, sortdb, chainstate, ibd)?;

        let Some(mut block_downloader) = self.block_downloader_nakamoto.take() else {
            return Ok(res);
        };

        for broken in block_downloader.neighbor_rpc.take_broken() {
            self.deregister_and_ban_neighbor(&broken);
        }

        for dead in block_downloader.neighbor_rpc.take_dead() {
            self.deregister_neighbor(&dead);
        }

        self.block_downloader_nakamoto = Some(block_downloader);
        Ok(res)
    }
}