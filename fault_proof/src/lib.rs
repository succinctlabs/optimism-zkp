pub mod config;
pub mod contract;
pub mod utils;

use alloy_eips::BlockNumberOrTag;
use alloy_network::Ethereum;
use alloy_primitives::{address, keccak256, Address, FixedBytes, B256, U256};
use alloy_provider::{
    fillers::{FillProvider, TxFiller},
    Provider, RootProvider,
};
use alloy_rpc_types_eth::Block;
use alloy_sol_types::SolValue;
use anyhow::{bail, Result};
use async_trait::async_trait;
use op_alloy_network::{primitives::BlockTransactionsKind, Optimism};
use op_alloy_rpc_types::Transaction;
use tokio::time::Duration;

use crate::contract::{
    AnchorStateRegistry, DisputeGameFactory::DisputeGameFactoryInstance, GameStatus, L2Output,
    OPSuccinctFaultDisputeGame, ProposalStatus,
};

pub type L1Provider = RootProvider;
pub type L2Provider = RootProvider<Optimism>;
pub type L2NodeProvider = RootProvider<Optimism>;
pub type L1ProviderWithWallet<F, P> = FillProvider<F, P, Ethereum>;

pub const NUM_CONFIRMATIONS: u64 = 3;
pub const TIMEOUT_SECONDS: u64 = 60;

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Proposer,
    Challenger,
}

#[async_trait]
pub trait L2ProviderTrait {
    /// Get the L2 block by number.
    async fn get_l2_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> Result<Block<Transaction>>;

    /// Get the L2 storage root for an address at a given block number.
    async fn get_l2_storage_root(
        &self,
        address: Address,
        block_number: BlockNumberOrTag,
    ) -> Result<B256>;

    /// Compute the output root at a given L2 block number.
    async fn compute_output_root_at_block(&self, l2_block_number: U256) -> Result<FixedBytes<32>>;
}

#[async_trait]
impl L2ProviderTrait for L2Provider {
    /// Get the L2 block by number.
    async fn get_l2_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> Result<Block<Transaction>> {
        let block = self
            .get_block_by_number(block_number, BlockTransactionsKind::Hashes)
            .await?;
        if let Some(block) = block {
            Ok(block)
        } else {
            bail!("Failed to get L2 block by number");
        }
    }

    /// Get the L2 storage root for an address at a given block number.
    async fn get_l2_storage_root(
        &self,
        address: Address,
        block_number: BlockNumberOrTag,
    ) -> Result<B256> {
        let storage_root = self
            .get_proof(address, Vec::new())
            .block_id(block_number.into())
            .await?
            .storage_hash;
        Ok(storage_root)
    }

    /// Compute the output root at a given L2 block number.
    ///
    /// Local implementation is used because the RPC method `optimism_outputAtBlock` can fail for older
    /// blocks if the L2 node isn't fully synced or has pruned historical state data.
    ///
    /// Common error: "missing trie node ... state is not available".
    async fn compute_output_root_at_block(&self, l2_block_number: U256) -> Result<FixedBytes<32>> {
        let l2_block = self
            .get_l2_block_by_number(BlockNumberOrTag::Number(l2_block_number.to::<u64>()))
            .await?;
        let l2_state_root = l2_block.header.state_root;
        let l2_claim_hash = l2_block.header.hash;
        let l2_storage_root = self
            .get_l2_storage_root(
                address!("0x4200000000000000000000000000000000000016"),
                BlockNumberOrTag::Number(l2_block_number.to::<u64>()),
            )
            .await?;

        let l2_claim_encoded = L2Output {
            zero: 0,
            l2_state_root: l2_state_root.0.into(),
            l2_storage_hash: l2_storage_root.0.into(),
            l2_claim_hash: l2_claim_hash.0.into(),
        };
        let l2_output_root = keccak256(l2_claim_encoded.abi_encode());
        Ok(l2_output_root)
    }
}

#[async_trait]
pub trait FactoryTrait<F, P>
where
    F: TxFiller,
    P: Provider + Clone,
{
    /// Fetches the bond required to create a game.
    async fn fetch_init_bond(&self, game_type: u32) -> Result<U256>;

    /// Fetches the proof reward required to challenge a game.
    async fn fetch_proof_reward(&self, game_type: u32) -> Result<U256>;

    /// Fetches the latest game index.
    async fn fetch_latest_game_index(&self) -> Result<Option<U256>>;

    /// Fetches the game address by index.
    async fn fetch_game_address_by_index(&self, game_index: U256) -> Result<Address>;

    /// Get the latest valid proposal.
    ///
    /// This function checks from the latest game to the earliest game, returning the latest valid proposal.
    async fn get_latest_valid_proposal(
        &self,
        l2_provider: L2Provider,
    ) -> Result<Option<(U256, U256)>>;

    /// Get the anchor L2 block number.
    ///
    /// This function returns the L2 block number of the anchor game for a given game type.
    async fn get_anchor_l2_block_number(&self, game_type: u32) -> Result<U256>;

    /// Get the oldest challengable game address
    ///
    /// This function checks a window of recent games, starting from
    /// (latest_game_index - max_games_to_check_for_challenge) up to latest_game_index
    async fn get_oldest_challengable_game_address(
        &self,
        max_games_to_check_for_challenge: u64,
        l1_provider: L1Provider,
        l2_provider: L2Provider,
    ) -> Result<Option<Address>>;

    /// Determines if we should attempt resolution or not. The `oldest_game_index` is configured
    /// to be `latest_game_index` - `max_games_to_check_for_resolution`.
    ///
    /// If the oldest game has no parent (i.e., it's a first game), we always attempt resolution.
    /// For other games, we only attempt resolution if the parent game is not in progress.
    ///
    /// NOTE(fakedev9999): Needs to be updated considering more complex cases where there are
    ///                    multiple branches of games.
    async fn should_attempt_resolution(&self, oldest_game_index: U256) -> Result<(bool, Address)>;

    /// Attempts to resolve a challenged game.
    ///
    /// This function checks if the game is in progress and challenged, and if so, attempts to resolve it.
    async fn try_resolve_games(
        &self,
        index: U256,
        mode: Mode,
        l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
        l2_provider: L2Provider,
    ) -> Result<()>;

    /// Attempts to resolve all challenged games that the challenger won, up to `max_games_to_check_for_resolution`.
    async fn resolve_games(
        &self,
        mode: Mode,
        max_games_to_check_for_resolution: u64,
        l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
        l2_provider: L2Provider,
    ) -> Result<()>;
}

#[async_trait]
impl<F, P> FactoryTrait<F, P> for DisputeGameFactoryInstance<(), L1ProviderWithWallet<F, P>>
where
    F: TxFiller,
    P: Provider + Clone,
{
    /// Fetches the bond required to create a game.
    async fn fetch_init_bond(&self, game_type: u32) -> Result<U256> {
        let init_bond = self.initBonds(game_type).call().await?;
        Ok(init_bond._0)
    }

    /// Fetches the proof reward required to challenge a game.
    async fn fetch_proof_reward(&self, game_type: u32) -> Result<U256> {
        let game_impl_address = self.gameImpls(game_type).call().await?._0;
        let game_impl = OPSuccinctFaultDisputeGame::new(game_impl_address, self.provider());
        let proof_reward = game_impl.proofReward().call().await?;
        Ok(proof_reward.proofReward_)
    }

    /// Fetches the latest game index.
    async fn fetch_latest_game_index(&self) -> Result<Option<U256>> {
        let game_count = self.gameCount().call().await?;

        if game_count.gameCount_ == U256::ZERO {
            tracing::debug!("No games exist yet");
            return Ok(None);
        }

        let latest_game_index = game_count.gameCount_ - U256::from(1);
        tracing::debug!("Latest game index: {:?}", latest_game_index);

        Ok(Some(latest_game_index))
    }

    /// Fetches the game address by index.
    async fn fetch_game_address_by_index(&self, game_index: U256) -> Result<Address> {
        let game = self.gameAtIndex(game_index).call().await?;
        Ok(game.proxy)
    }

    /// Get the latest valid proposal.
    ///
    /// This function checks from the latest game to the earliest game, returning the latest valid proposal.
    async fn get_latest_valid_proposal(
        &self,
        l2_provider: L2Provider,
    ) -> Result<Option<(U256, U256)>> {
        // Get latest game index, return None if no games exist.
        let Some(mut game_index) = self.fetch_latest_game_index().await? else {
            tracing::info!("No games exist yet");
            return Ok(None);
        };

        let mut block_number;

        // Loop through games in reverse order (latest to earliest) to find the most recent valid game
        loop {
            // Get the game contract for the current index.
            let game_address = self.fetch_game_address_by_index(game_index).await?;
            let game = OPSuccinctFaultDisputeGame::new(game_address, self.provider());

            // Get the L2 block number the game is proposing output for.
            block_number = game.l2BlockNumber().call().await?.l2BlockNumber_;
            tracing::debug!(
                "Checking if game {:?} at block {:?} is valid",
                game_address,
                block_number
            );

            // Get the output root the game is proposing.
            let game_claim = game.rootClaim().call().await?.rootClaim_;

            // Compute the actual output root at the L2 block number.
            let output_root = l2_provider
                .compute_output_root_at_block(block_number)
                .await?;

            // If the output root matches the game claim, we've found the latest valid proposal.
            if output_root == game_claim {
                break;
            }

            // If the output root doesn't match the game claim, we need to find earlier games.
            tracing::info!(
                "Output root {:?} is not same as game claim {:?}",
                output_root,
                game_claim
            );

            // If we've reached index 0 (the earliest game) and still haven't found a valid proposal.
            // Return `None` as no valid proposals were found.
            if game_index == U256::ZERO {
                tracing::info!("No valid proposals found after checking all games");
                return Ok(None);
            }

            // Decrement the game index to check the previous game.
            game_index -= U256::from(1);
        }

        tracing::info!(
            "Latest valid proposal at game index {:?} with l2 block number: {:?}",
            game_index,
            block_number
        );

        Ok(Some((block_number, game_index)))
    }

    /// Get the anchor L2 block number.
    ///
    /// This function returns the L2 block number of the anchor game for a given game type.
    async fn get_anchor_l2_block_number(&self, game_type: u32) -> Result<U256> {
        let game_impl_address = self.gameImpls(game_type).call().await?._0;
        let game_impl = OPSuccinctFaultDisputeGame::new(game_impl_address, self.provider());
        let anchor_state_registry = AnchorStateRegistry::new(
            game_impl.anchorStateRegistry().call().await?.registry_,
            self.provider(),
        );
        let anchor_l2_block_number = anchor_state_registry.getAnchorRoot().call().await?._1;
        Ok(anchor_l2_block_number)
    }

    /// Get the oldest challengable game address
    ///
    /// This function checks a window of recent games, starting from
    /// (latest_game_index - max_games_to_check_for_challenge) up to latest_game_index
    async fn get_oldest_challengable_game_address(
        &self,
        max_games_to_check_for_challenge: u64,
        l1_provider: L1Provider,
        l2_provider: L2Provider,
    ) -> Result<Option<Address>> {
        // Get latest game index, return None if no games exist
        let Some(latest_game_index) = self.fetch_latest_game_index().await? else {
            tracing::info!("No games exist yet");
            return Ok(None);
        };

        // Start from the latest game index - max_games_to_check_for_challenge
        let mut game_index =
            latest_game_index.saturating_sub(U256::from(max_games_to_check_for_challenge));
        let mut game_address;
        let mut block_number;

        loop {
            // If we've reached last index and still haven't found a valid proposal
            if game_index > latest_game_index {
                tracing::info!("No invalid proposals found after checking all games");
                return Ok(None);
            }

            game_address = self.fetch_game_address_by_index(game_index).await?;
            let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider.clone());

            let claim_data = game.claimData().call().await?.claimData_;
            if claim_data.status != ProposalStatus::Unchallenged {
                tracing::info!(
                    "Game {:?} at index {:?} is not unchallenged, not attempting to challenge",
                    game_address,
                    game_index
                );

                game_index += U256::from(1);
                continue;
            }

            // Check if the the game is still in the challenge window
            let current_timestamp = l2_provider
                .get_l2_block_by_number(BlockNumberOrTag::Latest)
                .await?
                .header
                .timestamp;
            let deadline = U256::from(claim_data.deadline).to::<u64>();
            if deadline < current_timestamp {
                tracing::info!(
                    "Game {:?} at index {:?} deadline {:?} has passed, not attempting to challenge",
                    game_address,
                    game_index,
                    deadline
                );
                game_index += U256::from(1);
                continue;
            }

            block_number = game.l2BlockNumber().call().await?.l2BlockNumber_;
            tracing::info!(
                "Checking if game {:?} at index {:?} for block {:?} is invalid",
                game_address,
                game_index,
                block_number
            );
            let game_claim = game.rootClaim().call().await?.rootClaim_;

            let output_root = l2_provider
                .compute_output_root_at_block(block_number)
                .await?;

            if output_root != game_claim {
                tracing::info!(
                    "Output root {:?} at block {:?} is not same as game claim {:?}",
                    output_root,
                    block_number,
                    game_claim
                );
                break;
            }

            game_index += U256::from(1);
        }

        tracing::info!(
            "Oldest challengable game {:?} at game index {:?} with l2 block number: {:?}",
            game_address,
            game_index,
            block_number
        );

        Ok(Some(game_address))
    }

    /// Determines if we should attempt resolution or not. The `oldest_game_index` is configured
    /// to be `latest_game_index` - `max_games_to_check_for_resolution`.
    ///
    /// If the oldest game has no parent (i.e., it's a first game), we always attempt resolution.
    /// For other games, we only attempt resolution if the parent game is not in progress.
    ///
    /// NOTE(fakedev9999): Needs to be updated considering more complex cases where there are
    ///                    multiple branches of games.
    async fn should_attempt_resolution(&self, oldest_game_index: U256) -> Result<(bool, Address)> {
        let oldest_game_address = self.fetch_game_address_by_index(oldest_game_index).await?;
        let oldest_game = OPSuccinctFaultDisputeGame::new(oldest_game_address, self.provider());
        let parent_game_index = oldest_game.claimData().call().await?.claimData_.parentIndex;

        // Always attempt resolution for first games (those with parent_game_index == u32::MAX)
        // For other games, only attempt if the oldest game's parent game is resolved
        if parent_game_index == u32::MAX {
            Ok((true, oldest_game_address))
        } else {
            let parent_game_address = self
                .fetch_game_address_by_index(U256::from(parent_game_index))
                .await?;
            let parent_game = OPSuccinctFaultDisputeGame::new(parent_game_address, self.provider());

            Ok((
                parent_game.status().call().await?.status_ != GameStatus::IN_PROGRESS,
                oldest_game_address,
            ))
        }
    }

    /// Attempts to resolve a challenged game.
    ///
    /// This function checks if the game is in progress and challenged, and if so, attempts to resolve it.
    async fn try_resolve_games(
        &self,
        index: U256,
        mode: Mode,
        l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
        l2_provider: L2Provider,
    ) -> Result<()> {
        let game_address = self.fetch_game_address_by_index(index).await?;
        let game = OPSuccinctFaultDisputeGame::new(game_address, l1_provider_with_wallet.clone());
        if game.status().call().await?.status_ != GameStatus::IN_PROGRESS {
            tracing::info!(
                "Game {:?} at index {:?} is not in progress, not attempting resolution",
                game_address,
                index
            );
            return Ok(());
        }

        let claim_data = game.claimData().call().await?.claimData_;
        match mode {
            Mode::Proposer => {
                if claim_data.status != ProposalStatus::Unchallenged {
                    tracing::info!(
                        "Game {:?} at index {:?} is not unchallenged, not attempting resolution",
                        game_address,
                        index
                    );
                    return Ok(());
                }
            }
            Mode::Challenger => {
                if claim_data.status != ProposalStatus::Challenged {
                    tracing::info!(
                        "Game {:?} at index {:?} is not challenged, not attempting resolution",
                        game_address,
                        index
                    );
                    return Ok(());
                }
            }
        }

        let current_timestamp = l2_provider
            .get_l2_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .header
            .timestamp;
        let deadline = U256::from(claim_data.deadline).to::<u64>();
        if deadline >= current_timestamp {
            tracing::info!(
                "Game {:?} at index {:?} deadline {:?} has not passed, not attempting resolution",
                game_address,
                index,
                deadline
            );
            return Ok(());
        }

        let contract = OPSuccinctFaultDisputeGame::new(game_address, self.provider());
        // TODO(fakedev9999): Potentially need to add a gas provider.
        let receipt = contract
            .resolve()
            .send()
            .await?
            .with_required_confirmations(NUM_CONFIRMATIONS)
            .with_timeout(Some(Duration::from_secs(TIMEOUT_SECONDS)))
            .get_receipt()
            .await?;
        tracing::info!(
            "Successfully resolved challenged game {:?} at index {:?} with tx {:?}",
            game_address,
            index,
            receipt.transaction_hash
        );
        Ok(())
    }

    /// Attempts to resolve all challenged games that the challenger won, up to `max_games_to_check_for_resolution`.
    async fn resolve_games(
        &self,
        mode: Mode,
        max_games_to_check_for_resolution: u64,
        l1_provider_with_wallet: L1ProviderWithWallet<F, P>,
        l2_provider: L2Provider,
    ) -> Result<()> {
        // Find latest game index, return early if no games exist
        let Some(latest_game_index) = self.fetch_latest_game_index().await? else {
            tracing::info!("No games exist, skipping resolution");
            return Ok(());
        };

        // If the oldest game's parent game is not resolved, we'll not attempt resolution.
        // Except for the game without a parent, which are first games.
        let oldest_game_index =
            latest_game_index.saturating_sub(U256::from(max_games_to_check_for_resolution));
        let games_to_check = latest_game_index.min(U256::from(max_games_to_check_for_resolution));

        let (should_attempt_resolution, game_address) =
            self.should_attempt_resolution(oldest_game_index).await?;

        if should_attempt_resolution {
            for i in 0..games_to_check.to::<u64>() {
                let index = oldest_game_index + U256::from(i);
                self.try_resolve_games(
                    index,
                    mode,
                    l1_provider_with_wallet.clone(),
                    l2_provider.clone(),
                )
                .await?;
            }
        } else {
            tracing::info!(
                "Oldest game {:?} at index {:?} is not resolved, not attempting resolution",
                game_address,
                oldest_game_index
            );
        }

        Ok(())
    }
}
