use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{network::ReceiptResponse, Network, Provider};
use anyhow::Result;
use op_succinct_client_utils::{boot::hash_rollup_config, types::u32_to_u8};
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use sp1_sdk::{
    network::{
        proto::network::{ExecutionStatus, FulfillmentStatus},
        FulfillmentStrategy,
    },
    HashableKey, NetworkProver, Prover, ProverClient, SP1Proof, SP1ProofMode,
    SP1ProofWithPublicValues, SP1ProvingKey, SP1VerifyingKey,
};
use std::{str::FromStr, sync::Arc, time::Duration};
use tracing::{debug, info};

use crate::{
    db::{DriverDBClient, OPSuccinctRequest, RequestMode, RequestStatus},
    find_gaps, get_latest_proposed_block_number, get_ranges_to_prove, OPSuccinctProofRequester,
    AGG_ELF, RANGE_ELF,
};

use op_succinct_host_utils::OPSuccinctL2OutputOracle::OPSuccinctL2OutputOracleInstance as OPSuccinctL2OOContract;

pub struct ContractConfig<P, N>
where
    P: Provider<N> + 'static,
    N: Network,
{
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub l2oo_contract: OPSuccinctL2OOContract<(), P, N>,
}

#[derive(Debug, Clone)]
pub struct CommitmentConfig {
    pub range_vkey_commitment: B256,
    pub agg_vkey_hash: B256,
    pub rollup_config_hash: B256,
}

pub struct ProgramConfig {
    pub range_vk: SP1VerifyingKey,
    pub range_pk: SP1ProvingKey,
    pub agg_vk: SP1VerifyingKey,
    pub agg_pk: SP1ProvingKey,
    pub commitments: CommitmentConfig,
}

pub struct RequesterConfig {
    pub l1_chain_id: i64,
    pub l2_chain_id: i64,
    pub l2oo_address: Address,
    pub dgf_address: Address,
    pub range_proof_interval: u64,
    pub submission_interval: u64,
    pub max_concurrent_witness_gen: u64,
    pub max_concurrent_proof_requests: u64,
    pub range_proof_strategy: FulfillmentStrategy,
    pub agg_proof_strategy: FulfillmentStrategy,
    pub agg_proof_mode: SP1ProofMode,
    pub mock: bool,
}

pub struct DriverConfig {
    // // ****
    // // Proposer configuration
    // // ****
    pub network_prover: Arc<NetworkProver>,
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    pub range_proof_size: u64,
    pub submission_interval: u64,
    pub proof_timeout: u64,
    pub driver_db_client: Arc<DriverDBClient>,
    /// Limits on the maximum number of concurrent proof requests and witness generation requests.
    pub max_concurrent_proof_requests: u64,
    pub max_concurrent_witness_gen: u64,
}

pub struct Proposer<P, N>
where
    P: Provider<N> + 'static,
    N: Network,
{
    driver_config: DriverConfig,
    contract_config: ContractConfig<P, N>,
    program_config: Arc<ProgramConfig>,
    requester_config: RequesterConfig,
    proof_requester: Arc<OPSuccinctProofRequester>,
}

// 5 confirmations (1 minute)
const NUM_CONFIRMATIONS: u64 = 5;
// 2 minute timeout.
const TIMEOUT: u64 = 120;

// TODO: Add support for DGF.
impl<P, N> Proposer<P, N>
where
    P: Provider<N> + 'static,
    N: Network,
{
    pub async fn new(
        provider: P,
        db_client: Arc<DriverDBClient>,
        fetcher: Arc<OPSuccinctDataFetcher>,
        config: RequesterConfig,
    ) -> Result<Self> {
        let network_prover = Arc::new(ProverClient::builder().network().build());
        let (range_pk, range_vk) = network_prover.setup(RANGE_ELF);
        let (agg_pk, agg_vk) = network_prover.setup(AGG_ELF);
        let multi_block_vkey_u8 = u32_to_u8(range_vk.vk.hash_u32());
        let range_vkey_commitment = B256::from(multi_block_vkey_u8);
        let agg_vkey_hash = B256::from_str(&agg_vk.bytes32()).unwrap();

        // Initialize fetcher
        let rollup_config_hash = hash_rollup_config(fetcher.rollup_config.as_ref().unwrap());

        // Use config values instead of env vars
        let range_proof_strategy = config.range_proof_strategy;
        let agg_proof_strategy = config.agg_proof_strategy;
        let agg_proof_mode = config.agg_proof_mode;

        let l2oo_address = config.l2oo_address;
        let dgf_address = config.dgf_address;

        let range_proof_interval = config.range_proof_interval;
        let submission_interval = config.submission_interval;
        let max_concurrent_witness_gen = config.max_concurrent_witness_gen;
        let max_concurrent_proof_requests = config.max_concurrent_proof_requests;
        let mock = config.mock;
        const PROOF_TIMEOUT: u64 = 60 * 60;

        let program_config = Arc::new(ProgramConfig {
            range_vk,
            range_pk,
            agg_vk,
            agg_pk,
            commitments: CommitmentConfig {
                range_vkey_commitment,
                agg_vkey_hash,
                rollup_config_hash,
            },
        });

        // Initialize the proof requester.
        let proof_requester = Arc::new(OPSuccinctProofRequester::new(
            network_prover.clone(),
            fetcher.clone(),
            db_client.clone(),
            program_config.clone(),
            mock,
            range_proof_strategy,
            agg_proof_strategy,
            agg_proof_mode,
        ));

        let l1_chain_id = fetcher.l1_provider.get_chain_id().await?;
        let l2_chain_id = fetcher.l2_provider.get_chain_id().await?;

        let l2oo_contract = OPSuccinctL2OOContract::new(l2oo_address, provider);

        let proposer = Proposer {
            driver_config: DriverConfig {
                network_prover,
                fetcher,
                range_proof_size: 0,
                submission_interval: 0,
                proof_timeout: PROOF_TIMEOUT,
                driver_db_client: db_client,
                max_concurrent_proof_requests,
                max_concurrent_witness_gen,
            },
            contract_config: ContractConfig {
                l2oo_address,
                dgf_address,
                l2oo_contract,
            },
            program_config: program_config,
            requester_config: RequesterConfig {
                l2oo_address,
                dgf_address,
                range_proof_interval,
                submission_interval,
                range_proof_strategy,
                agg_proof_strategy,
                agg_proof_mode,
                max_concurrent_witness_gen,
                max_concurrent_proof_requests,
                l1_chain_id: l1_chain_id as i64,
                l2_chain_id: l2_chain_id as i64,
                mock,
            },
            proof_requester,
        };
        Ok(proposer)
    }

    /// Use the in-memory index of the highest block number to add new ranges to the database.
    #[tracing::instrument(name = "proposer.add_new_ranges", skip(self))]
    pub async fn add_new_ranges(&self) -> Result<()> {
        let latest_finalized_header = self
            .driver_config
            .fetcher
            .get_l2_header(BlockId::finalized())
            .await?;

        // Get the highest block number of any of range request in the database that is not FAILED or CANCELLED or RELAYED with the same commitment.
        let highest_request = self
            .driver_config
            .driver_db_client
            .fetch_highest_range_request_with_statuses_and_commitment(
                &[
                    RequestStatus::Unrequested,
                    RequestStatus::WitnessGeneration,
                    RequestStatus::Execution,
                    RequestStatus::Complete,
                    RequestStatus::Prove,
                ],
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // If there are no requests in the database, the current processed block number is the latest finalized block number on the contract. Otherwise, it's the highest block number
        // of any of the requests in the database that are not FAILED or CANCELLED.
        let mut current_processed_block = match highest_request {
            Some(request) => request.end_block as u64,
            None => {
                tracing::debug!(
                    "No requests in the database, using latest proposed block number on contract."
                );
                get_latest_proposed_block_number(
                    self.contract_config.l2oo_address,
                    self.driver_config.fetcher.as_ref(),
                )
                .await?
            }
        };
        tracing::debug!("Current processed block: {}.", current_processed_block);

        let mut requests = Vec::new();

        // Only add new ranges if the current block is less than the latest finalized block minus the
        // range proof interval. This ensures that only ranges that cover range_proof_interval blocks
        // are added to the database.
        while current_processed_block
            < latest_finalized_header.number - self.requester_config.range_proof_interval
        {
            let end_block = std::cmp::min(
                current_processed_block + self.requester_config.range_proof_interval,
                latest_finalized_header.number,
            );

            let request = OPSuccinctRequest::create_range_request(
                if self.requester_config.mock {
                    RequestMode::Mock
                } else {
                    RequestMode::Real
                },
                current_processed_block as i64,
                end_block as i64,
                self.program_config.commitments.range_vkey_commitment.into(),
                self.program_config.commitments.rollup_config_hash.into(),
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
                self.driver_config.fetcher.clone(),
            )
            .await?;

            requests.push(request);
            current_processed_block = end_block;
        }

        if !requests.is_empty() {
            tracing::debug!("Inserting {} requests into the database.", requests.len());
            self.driver_config
                .driver_db_client
                .insert_requests(&requests)
                .await?;
        }

        Ok(())
    }

    /// Handle all proof requests in the Prove state.
    #[tracing::instrument(name = "proposer.handle_proving_requests", skip(self))]
    pub async fn handle_proving_requests(&self) -> Result<()> {
        // Get all requests from the database.
        let prove_requests = self
            .driver_config
            .driver_db_client
            .fetch_requests_by_status(
                RequestStatus::Prove,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        debug!(
            "Getting proof statuses for {} requests.",
            prove_requests.len()
        );

        // Get the proof status of all of the requests in parallel.
        futures_util::future::join_all(
            prove_requests
                .into_iter()
                .map(|request| async move { self.process_proof_request_status(request).await }),
        )
        .await;

        Ok(())
    }

    /// Process a single OP Succinct request's proof status.
    #[tracing::instrument(name = "proposer.process_proof_request_status", skip(self))]
    pub async fn process_proof_request_status(&self, request: OPSuccinctRequest) -> Result<()> {
        if let Some(proof_request_id) = request.proof_request_id.as_ref() {
            let (status, proof) = self
                .driver_config
                .network_prover
                .get_proof_status(B256::from_slice(&proof_request_id))
                .await?;

            let execution_status = ExecutionStatus::try_from(status.execution_status).unwrap();
            let fulfillment_status =
                FulfillmentStatus::try_from(status.fulfillment_status).unwrap();

            // If the proof request has been fulfilled, update the request to status Complete and add the proof bytes to the database.
            if fulfillment_status == FulfillmentStatus::Fulfilled {
                let proof: SP1ProofWithPublicValues = proof.unwrap();

                let proof_bytes = match proof.proof {
                    // If it's a compressed proof, serialize with bincode.
                    SP1Proof::Compressed(_) => bincode::serialize(&proof).unwrap(),
                    // If it's Groth16 or PLONK, get the on-chain proof bytes.
                    _ => proof.bytes(),
                };

                // Add the completed proof to the database.
                self.driver_config
                    .driver_db_client
                    .update_proof_to_complete(request.id, &proof_bytes)
                    .await?;
                // Update the prove_duration based on the current time and the proof_request_time.
                self.driver_config
                    .driver_db_client
                    .update_prove_duration(request.id)
                    .await?;
            } else if status.fulfillment_status == FulfillmentStatus::Unfulfillable as i32 {
                self.proof_requester
                    .retry_request(&request, execution_status)
                    .await?;
            }
        } else {
            // There should never be a proof request in Prove status without a proof request id.
            tracing::warn!("Request has no proof request id: {:?}", request);
        }

        Ok(())
    }

    /// Create aggregation proofs based on the completed range proofs. The range proofs must be contiguous and have
    /// the same range vkey commitment. Assumes that the range proof retry logic guarantees that there is not
    /// two potential contiguous chains of range proofs.
    ///
    /// Only creates an Aggregation proof if there's not an Aggregation proof in progress with the same start block.
    #[tracing::instrument(name = "proposer.create_aggregation_proofs", skip(self))]
    pub async fn create_aggregation_proofs(&self) -> Result<()> {
        // Check if there's an Aggregation proof with the same start block AND range verification key commitment AND aggregation vkey.
        // If so, return.
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        // Get all active Aggregation proofs with the same start block, range vkey commitment, and aggregation vkey.
        let agg_proofs = self
            .driver_config
            .driver_db_client
            .fetch_active_agg_proofs(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        if agg_proofs.len() > 0 {
            tracing::debug!("There is already an Aggregation proof queued with the same start block, range vkey commitment, and aggregation vkey.");
            return Ok(());
        }

        // Get the latest proposed block number on the contract.
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        // Get all completed range proofs from the database.
        let completed_range_proofs = self
            .driver_config
            .driver_db_client
            .fetch_completed_range_proofs(
                &self.program_config.commitments,
                latest_proposed_block_number as i64,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // Get the largest contiguous range of completed range proofs.
        let largest_contiguous_range = self.get_largest_contiguous_range(completed_range_proofs)?;

        // Get the submission interval from the contract.
        let contract_submission_interval: u64 = self
            .contract_config
            .l2oo_contract
            .submissionInterval()
            .call()
            .await?
            .submissionInterval
            .try_into()
            .unwrap();

        // Use the submission interval from the contract if it's greater than the one in the proposer config.
        let submission_interval =
            contract_submission_interval.max(self.requester_config.submission_interval);

        debug!(
            "Submission interval for aggregation proof: {}.",
            submission_interval
        );

        if let Some(last_request) = largest_contiguous_range.last() {
            if (last_request.end_block - last_request.start_block) as u64 >= submission_interval {
                // If an aggregation request with the same start block and end block and commitment config exists, there's no need to checkpoint the L1 block hash.
                // Use the existing L1 block hash from the existing request.
                let existing_request = self
                    .driver_config
                    .driver_db_client
                    .fetch_agg_request_with_checkpointed_block_hash(
                        last_request.start_block,
                        last_request.end_block,
                        &self.program_config.commitments,
                        self.requester_config.l1_chain_id as i64,
                        self.requester_config.l2_chain_id as i64,
                    )
                    .await?;

                // If there's an existing aggregation request with the same start block, end block, and commitment config that has a checkpointed block hash, use the existing L1 block hash and number. This is
                // likely caused by an error generating the aggregation proof, but there's no need to checkpoint the L1 block hash again.
                let (checkpointed_l1_block_hash, checkpointed_l1_block_number) = if let Some(
                    existing_request,
                ) =
                    existing_request
                {
                    tracing::debug!("Found existing aggregation request with the same start block, end block, and commitment config that has a checkpointed block hash.");
                    (
                        B256::from_slice(
                            &existing_request
                                .checkpointed_l1_block_hash
                                .expect("checkpointed_l1_block_hash is None"),
                        ),
                        existing_request
                            .checkpointed_l1_block_number
                            .expect("checkpointed_l1_block_number is None"),
                    )
                } else {
                    // Checkpoint an L1 block hash that will be used to create the aggregation proof.
                    let latest_header = self
                        .driver_config
                        .fetcher
                        .get_l1_header(BlockId::latest())
                        .await?;

                    // Checkpoint the L1 block hash.
                    let receipt = self
                        .contract_config
                        .l2oo_contract
                        .checkpointBlockHash(U256::from(latest_header.number))
                        .send()
                        .await?
                        .with_required_confirmations(NUM_CONFIRMATIONS)
                        .with_timeout(Some(Duration::from_secs(TIMEOUT)))
                        .get_receipt()
                        .await?;

                    // If transaction reverted, log the error.
                    if !receipt.status() {
                        tracing::error!("Transaction reverted: {:?}", receipt);
                    }

                    tracing::info!("Checkpointed L1 block number: {:?}.", latest_header.number);

                    (latest_header.hash_slow(), latest_header.number as i64)
                };

                // Create an aggregation proof request to cover the range with the checkpointed L1 block hash.
                self.driver_config
                    .driver_db_client
                    .insert_request(&OPSuccinctRequest::new_agg_request(
                        last_request.mode,
                        latest_proposed_block_number as i64,
                        last_request.end_block,
                        self.program_config.commitments.range_vkey_commitment,
                        self.program_config.commitments.agg_vkey_hash,
                        self.program_config.commitments.rollup_config_hash,
                        self.requester_config.l1_chain_id as i64,
                        self.requester_config.l2_chain_id as i64,
                        checkpointed_l1_block_number,
                        checkpointed_l1_block_hash,
                    ))
                    .await?;
            }
        }

        Ok(())
    }

    /// Request all unrequested proofs up to MAX_CONCURRENT_PROOF_REQUESTS. If there are already MAX_CONCURRENT_PROOF_REQUESTS proofs in WitnessGeneration, Execute, and Prove status, return.
    /// If there are already MAX_CONCURRENT_WITNESS_GEN proofs in WitnessGeneration or Execute status, return.
    ///
    /// TODO: Submit up to MAX_CONCURRENT_PROOF_REQUESTS at a time. Don't do one per loop.
    #[tracing::instrument(name = "proposer.request_queued_proofs", skip(self))]
    async fn request_queued_proofs(&self) -> Result<()> {
        let requests = self
            .driver_config
            .driver_db_client
            .fetch_requests_with_status(
                &[
                    RequestStatus::WitnessGeneration,
                    RequestStatus::Execution,
                    RequestStatus::Prove,
                ],
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // If there are already MAX_CONCURRENT_PROOF_REQUESTS proofs in WitnessGeneration, Execute, and Prove status, return.
        if requests.len() >= self.driver_config.max_concurrent_proof_requests as usize {
            debug!("There are already MAX_CONCURRENT_PROOF_REQUESTS proofs in WitnessGeneration, Execute, and Prove status.");
            return Ok(());
        }

        // If there are already MAX_CONCURRENT_WITNESS_GEN proofs in WitnessGeneration or Execute status, return.
        if requests
            .iter()
            .filter(|r| {
                r.status == RequestStatus::WitnessGeneration || r.status == RequestStatus::Execution
            })
            .count()
            >= self.driver_config.max_concurrent_witness_gen as usize
        {
            debug!("There are already MAX_CONCURRENT_WITNESS_GEN proofs in WitnessGeneration or Execute status.");
            return Ok(());
        }

        // Get the next proof to request.
        let next_request = self.get_next_unrequested_proof().await?;

        if let Some(request) = next_request {
            info!(
                request_id = request.id,
                request_type = ?request.req_type,
                start_block = request.start_block,
                end_block = request.end_block,
                "Making proof request"
            );

            let proof_requester = self.proof_requester.clone();

            // Spawn a task to handle the proof request lifecycle.
            tokio::spawn(async move {
                if let Err(e) = proof_requester.make_proof_request(request.clone()).await {
                    // If the proof request failed, retry it.
                    tracing::error!("Failed to make proof request: {}", e);
                    if let Err(e) = proof_requester
                        .retry_request(&request, ExecutionStatus::UnspecifiedExecutionStatus)
                        .await
                    {
                        tracing::error!("Failed to retry proof request: {}", e);
                    }
                }
            });
        }

        Ok(())
    }

    /// Get the next unrequested proof from the database.
    ///
    /// If there is an Aggregation proof with the same start block, range vkey commitment, and aggregation vkey, return that.
    /// Otherwise, return a range proof with the lowest start block.
    async fn get_next_unrequested_proof(&self) -> Result<Option<OPSuccinctRequest>> {
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        let unreq_agg_request = self
            .driver_config
            .driver_db_client
            .fetch_unrequested_agg_proof(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        if let Some(unreq_agg_request) = unreq_agg_request {
            return Ok(Some(unreq_agg_request));
        }

        let unreq_range_request = self
            .driver_config
            .driver_db_client
            .fetch_unrequested_range_proofs(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        if let Some(unreq_range_request) = unreq_range_request {
            return Ok(Some(unreq_range_request));
        }

        Ok(None)
    }

    /// Get the largest contiguous range of completed range proofs.
    fn get_largest_contiguous_range(
        &self,
        completed_range_proofs: Vec<OPSuccinctRequest>,
    ) -> Result<Vec<OPSuccinctRequest>> {
        let mut largest_contiguous_range: Vec<OPSuccinctRequest> = Vec::new();

        for proof in completed_range_proofs {
            if largest_contiguous_range.is_empty() {
                largest_contiguous_range.push(proof);
            } else if proof.start_block == largest_contiguous_range.last().unwrap().end_block {
                largest_contiguous_range.push(proof);
            } else {
                break;
            }
        }

        Ok(largest_contiguous_range)
    }

    /// Submit all completed aggregation proofs to the prover network.
    #[tracing::instrument(name = "proposer.submit_agg_proofs", skip(self))]
    async fn submit_agg_proofs(&self) -> Result<()> {
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        // See if there is an aggregation proof that is complete for this start block. NOTE: There should only be one "pending" aggregation proof at a time for a specific start block.
        let completed_agg_proof = self
            .driver_config
            .driver_db_client
            .fetch_completed_aggregation_proofs(
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // If there are no completed aggregation proofs, do nothing.
        let completed_agg_proof = match completed_agg_proof {
            Some(proof) => proof,
            None => return Ok(()),
        };

        // Get the output at the end block of the last completed aggregation proof.
        let output = self
            .driver_config
            .fetcher
            .get_l2_output_at_block(completed_agg_proof.end_block as u64)
            .await?;

        // Propose the L2 output.
        let receipt = self
            .contract_config
            .l2oo_contract
            .proposeL2Output(
                output.output_root,
                U256::from(completed_agg_proof.end_block),
                U256::from(completed_agg_proof.checkpointed_l1_block_number.unwrap()),
                completed_agg_proof.proof.unwrap().into(),
            )
            .send()
            .await?
            .with_required_confirmations(NUM_CONFIRMATIONS)
            .with_timeout(Some(Duration::from_secs(TIMEOUT)))
            .get_receipt()
            .await?;

        // If the transaction reverted, log the error.
        if !receipt.status() {
            tracing::error!("Transaction reverted: {:?}", receipt);
        }

        info!(
            "Relayed aggregation proof. Transaction hash: {:?}",
            receipt.transaction_hash()
        );

        // Update the request to status RELAYED.
        self.driver_config
            .driver_db_client
            .update_request_to_relayed(
                completed_agg_proof.id,
                receipt.transaction_hash().into(),
                self.contract_config.l2oo_address.into(),
            )
            .await?;

        Ok(())
    }

    /// Validate the requester config matches the contract.
    async fn validate_contract_config(&self) -> Result<()> {
        // Validate the requester config matches the contract.
        let contract_rollup_config_hash = self
            .contract_config
            .l2oo_contract
            .rollupConfigHash()
            .call()
            .await?
            .rollupConfigHash;
        let contract_agg_vkey_hash = self
            .contract_config
            .l2oo_contract
            .aggregationVkey()
            .call()
            .await?
            .aggregationVkey;
        let contract_range_vkey_commitment = self
            .contract_config
            .l2oo_contract
            .rangeVkeyCommitment()
            .call()
            .await?
            .rangeVkeyCommitment;

        let rollup_config_hash_match =
            contract_rollup_config_hash == self.program_config.commitments.rollup_config_hash;
        let agg_vkey_hash_match =
            contract_agg_vkey_hash == self.program_config.commitments.agg_vkey_hash;
        let range_vkey_commitment_match =
            contract_range_vkey_commitment == self.program_config.commitments.range_vkey_commitment;

        if !rollup_config_hash_match || !agg_vkey_hash_match || !range_vkey_commitment_match {
            tracing::error!(
                rollup_config_hash_match = rollup_config_hash_match,
                agg_vkey_hash_match = agg_vkey_hash_match,
                range_vkey_commitment_match = range_vkey_commitment_match,
                "Config mismatches detected."
            );
            return Err(anyhow::anyhow!("Config mismatches detected."));
        }

        Ok(())
    }

    /// Initialize the proposer by cleaning up stale requests and creating new range proof requests for the proposer with the given chain ID.
    ///
    /// This function performs several key tasks:
    /// 1. Validates that the proposer's config matches the contract
    /// 2. Deletes unrecoverable requests (UNREQUESTED, EXECUTION, WITNESS_GENERATION)
    /// 3. Cancels PROVE requests with mismatched commitment configs
    /// 4. Identifies gaps between the latest proposed block and finalized block
    /// 5. Creates new range proof requests to cover those gaps
    ///
    /// The goal is to ensure the database is in a clean state and all block ranges
    /// between the latest proposed block and finalized block have corresponding requests.
    #[tracing::instrument(name = "proposer.initialize_proposer", skip(self))]
    async fn initialize_proposer(&self) -> Result<()> {
        // Validate the requester config matches the contract.
        self.validate_contract_config().await?;

        // Delete all requests for the same chain ID that are of status UNREQUESTED, EXECUTION or WITNESS_GENERATION as they're unrecoverable.
        self.driver_config
            .driver_db_client
            .delete_all_requests_with_statuses(
                &[
                    RequestStatus::Unrequested,
                    RequestStatus::Execution,
                    RequestStatus::WitnessGeneration,
                ],
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // Cancel all requests in PROVE state for the same chain ID that have a different commitment config.
        self.driver_config
            .driver_db_client
            .cancel_prove_requests_with_different_commitment_config(
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // Get the latest proposed block number on the contract.
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        let finalized_block_number = self
            .driver_config
            .fetcher
            .get_l2_header(BlockId::finalized())
            .await?
            .number;

        // Get all requests in PROVE or COMPLETE status with the same commitment config and start block >= latest_proposed_block_number.
        // These requests are non-overlapping.
        let mut requests = self
            .driver_config
            .driver_db_client
            .fetch_range_requests_with_status_and_start_block(
                &[RequestStatus::Prove, RequestStatus::Complete],
                latest_proposed_block_number as i64,
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // Sort the requests by start block.
        requests.sort_by_key(|r| r.start_block);

        let disjoint_ranges = find_gaps(
            latest_proposed_block_number as i64,
            finalized_block_number as i64,
            &requests
                .iter()
                .map(|r| (r.start_block, r.end_block))
                .collect::<Vec<(i64, i64)>>(),
        );

        let ranges_to_prove = get_ranges_to_prove(
            &disjoint_ranges,
            self.requester_config.range_proof_interval as i64,
        );

        // Create range proof requests for the ranges to prove.
        let mut new_range_requests = Vec::new();
        for range in ranges_to_prove {
            new_range_requests.push(
                OPSuccinctRequest::create_range_request(
                    if self.requester_config.mock {
                        RequestMode::Mock
                    } else {
                        RequestMode::Real
                    },
                    range.0,
                    range.1,
                    self.program_config.commitments.range_vkey_commitment,
                    self.program_config.commitments.rollup_config_hash,
                    self.requester_config.l1_chain_id as i64,
                    self.requester_config.l2_chain_id as i64,
                    self.driver_config.fetcher.clone(),
                )
                .await?,
            );
        }

        // Insert the new range proof requests into the database.
        self.driver_config
            .driver_db_client
            .insert_requests(&new_range_requests)
            .await?;

        Ok(())
    }

    /// Fetch and log the proposer metrics.
    async fn log_proposer_metrics(&self) -> Result<()> {
        // Get the latest proposed block number on the contract.
        let latest_proposed_block_number = get_latest_proposed_block_number(
            self.contract_config.l2oo_address,
            self.driver_config.fetcher.as_ref(),
        )
        .await?;

        // Get all completed range proofs from the database.
        let completed_range_proofs = self
            .driver_config
            .driver_db_client
            .fetch_completed_range_proofs(
                &self.program_config.commitments,
                latest_proposed_block_number as i64,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        // Get the largest contiguous range of completed range proofs.
        let largest_contiguous_range = self.get_largest_contiguous_range(completed_range_proofs)?;

        let highest_block_number = if largest_contiguous_range.is_empty() {
            latest_proposed_block_number
        } else {
            largest_contiguous_range.last().unwrap().end_block as u64
        };

        let requests = self
            .driver_config
            .driver_db_client
            .fetch_requests_with_status(
                &[
                    RequestStatus::Unrequested,
                    RequestStatus::Prove,
                    RequestStatus::Execution,
                    RequestStatus::WitnessGeneration,
                ],
                &self.program_config.commitments,
                self.requester_config.l1_chain_id as i64,
                self.requester_config.l2_chain_id as i64,
            )
            .await?;

        let num_unrequested_requests = requests
            .iter()
            .filter(|r| r.status == RequestStatus::Unrequested)
            .count();
        let num_prove_requests = requests
            .iter()
            .filter(|r| r.status == RequestStatus::Prove)
            .count();
        let num_execution_requests = requests
            .iter()
            .filter(|r| r.status == RequestStatus::Execution)
            .count();
        let num_witness_generation_requests = requests
            .iter()
            .filter(|r| r.status == RequestStatus::WitnessGeneration)
            .count();
        info!(target: "proposer_metrics",
            "unrequested={num_unrequested_requests} prove={num_prove_requests} execution={num_execution_requests} witness_generation={num_witness_generation_requests} highest_contiguous_proven_block={highest_block_number}"
        );

        Ok(())
    }

    #[tracing::instrument(name = "proposer.run", skip(self))]
    pub async fn run(&self) -> Result<()> {
        // Handle the case where the proposer is being re-started and the proposer state needs to be updated.
        self.initialize_proposer().await?;

        // Loop interval in seconds.
        const PROPOSER_LOOP_INTERVAL: u64 = 60;
        loop {
            // Validate the requester config matches the contract.
            self.validate_contract_config().await?;

            // Log the proposer metrics.
            self.log_proposer_metrics().await?;

            // Add new ranges to the database.
            self.add_new_ranges().await?;

            // Get all proof statuses of all requests in the proving state.
            self.handle_proving_requests().await?;

            // Create aggregation proofs based on the completed range proofs. Checkpoints the block hash associated with the aggregation proof
            // in advance.
            self.create_aggregation_proofs().await?;

            // Request all unrequested proofs from the prover network.
            self.request_queued_proofs().await?;

            // Determine if any aggregation proofs that are complete need to be checkpointed.
            self.submit_agg_proofs().await?;

            // Sleep for the proposer loop interval.
            tokio::time::sleep(Duration::from_secs(PROPOSER_LOOP_INTERVAL)).await;
        }
    }

    pub async fn stop(&self) -> Result<()> {
        Ok(())
    }
}
