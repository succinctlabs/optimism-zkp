use alloy_primitives::B256;
use alloy_provider::Provider;
use anyhow::{Context, Result};
use chrono::Local;
use op_succinct_client_utils::boot::BootInfoStruct;
use op_succinct_host_utils::{
    fetcher::{CacheMode, OPSuccinctDataFetcher},
    get_agg_proof_stdin, get_proof_stdin, start_server_and_native_client, ProgramType,
};
use sp1_sdk::{
    network::{proto::network::ExecutionStatus, FulfillmentStrategy},
    NetworkProver, ProverClient, SP1Proof, SP1ProofMode, SP1ProofWithPublicValues, SP1Stdin,
    SP1_CIRCUIT_VERSION,
};
use std::{sync::Arc, time::Instant};
use tracing::{error, info};

use crate::db::DriverDBClient;
use crate::RANGE_ELF;
use crate::{
    OPSuccinctRequest, ProgramConfig, RequestExecutionStatistics, RequestStatus, RequestType,
};

pub struct OPSuccinctProofRequester {
    pub network_prover: Arc<NetworkProver>,
    pub fetcher: Arc<OPSuccinctDataFetcher>,
    pub db_client: Arc<DriverDBClient>,
    pub program_config: Arc<ProgramConfig>,
    pub mock: bool,
    pub range_strategy: FulfillmentStrategy,
    pub agg_strategy: FulfillmentStrategy,
    pub agg_mode: SP1ProofMode,
}

impl OPSuccinctProofRequester {
    pub fn new(
        network_prover: Arc<NetworkProver>,
        fetcher: Arc<OPSuccinctDataFetcher>,
        db_client: Arc<DriverDBClient>,
        program_config: Arc<ProgramConfig>,
        mock: bool,
        range_strategy: FulfillmentStrategy,
        agg_strategy: FulfillmentStrategy,
        agg_mode: SP1ProofMode,
    ) -> Self {
        Self {
            network_prover,
            fetcher,
            db_client,
            program_config,
            mock,
            range_strategy,
            agg_strategy,
            agg_mode,
        }
    }

    /// Generates the witness for a range proof.
    pub async fn range_proof_witnessgen(&self, request: &OPSuccinctRequest) -> Result<SP1Stdin> {
        let host_args = self
            .fetcher
            .get_host_args(
                request.start_block as u64,
                request.end_block as u64,
                None,
                ProgramType::Multi,
                CacheMode::DeleteCache,
            )
            .await
            .context("Failed to get host CLI args")?;

        let mem_kv_store = start_server_and_native_client(host_args).await?;
        let sp1_stdin = get_proof_stdin(mem_kv_store).context("Failed to get proof stdin")?;

        Ok(sp1_stdin)
    }

    /// Generates the witness for an aggregation proof.
    pub async fn agg_proof_witnessgen(
        &self,
        request: &OPSuccinctRequest,
        mut range_proofs: Vec<SP1ProofWithPublicValues>,
    ) -> Result<SP1Stdin> {
        let boot_infos: Vec<BootInfoStruct> = range_proofs
            .iter_mut()
            .map(|proof| proof.public_values.read())
            .collect();

        let proofs: Vec<SP1Proof> = range_proofs
            .iter_mut()
            .map(|proof| proof.proof.clone())
            .collect();

        let l1_head = request
            .checkpointed_l1_block_hash
            .as_ref()
            .expect("Aggregation proof has no checkpointed block.");

        let headers = self
            .fetcher
            .get_header_preimages(&boot_infos, B256::from_slice(&l1_head))
            .await?;

        let stdin = get_agg_proof_stdin(
            proofs,
            boot_infos,
            headers,
            &self.program_config.range_vk,
            B256::from_slice(&l1_head),
        )?;

        Ok(stdin)
    }

    /// Requests a range proof via the network prover.
    pub async fn request_range_proof(&self, stdin: SP1Stdin) -> Result<B256> {
        self.network_prover
            .prove(&self.program_config.range_pk, &stdin)
            .compressed()
            .strategy(self.range_strategy)
            .skip_simulation(true)
            .cycle_limit(1_000_000_000_000)
            .request_async()
            .await
    }

    /// Requests an aggregation proof via the network prover.
    pub async fn request_agg_proof(&self, stdin: SP1Stdin) -> Result<B256> {
        self.network_prover
            .prove(&self.program_config.agg_pk, &stdin)
            .mode(self.agg_mode)
            .strategy(self.agg_strategy)
            .request_async()
            .await
    }

    /// Generates a mock range proof and writes the execution statistics to the database.
    pub async fn request_mock_range_proof(
        &self,
        request: &OPSuccinctRequest,
        stdin: SP1Stdin,
    ) -> Result<SP1ProofWithPublicValues> {
        info!(
            request_id = request.id,
            request_type = ?request.req_type,
            start_block = request.start_block,
            end_block = request.end_block,
            "Executing mock range proof"
        );

        let start_time = Instant::now();
        let (pv, report) = self.network_prover.execute(RANGE_ELF, &stdin).run()?;
        let execution_duration = start_time.elapsed().as_secs();

        info!(
            request_id = request.id,
            request_type = ?request.req_type,
            start_block = request.start_block,
            end_block = request.end_block,
            duration_s = execution_duration,
            "Executed mock range proof.",
        );

        let execution_statistics = RequestExecutionStatistics::new(report);

        // Write the execution data to the database.
        self.db_client
            .insert_execution_statistics(
                request.id,
                serde_json::to_value(execution_statistics)?,
                execution_duration as i64,
            )
            .await?;

        Ok(SP1ProofWithPublicValues::create_mock_proof(
            &self.program_config.range_pk,
            pv.clone(),
            SP1ProofMode::Compressed,
            SP1_CIRCUIT_VERSION,
        ))
    }

    /// Generates a mock aggregation proof.
    pub async fn request_mock_agg_proof(
        &self,
        stdin: SP1Stdin,
    ) -> Result<SP1ProofWithPublicValues> {
        let prover = ProverClient::builder().mock().build();
        // TODO: Potentially add execution statistics in the future.
        prover
            .prove(&self.program_config.agg_pk, &stdin)
            .mode(self.agg_mode)
            .deferred_proof_verification(false)
            .run()
    }

    /// Handles a failed proof request by either splitting range requests or re-queuing the same one.
    pub async fn retry_request(
        &self,
        request: &OPSuccinctRequest,
        execution_status: ExecutionStatus,
    ) -> Result<()> {
        info!("Retrying request: {:?}", request);

        // Mark the existing request as failed.
        self.db_client
            .update_request_status(request.id, RequestStatus::Failed)
            .await?;

        let l1_chain_id = self.fetcher.l1_provider.get_chain_id().await?;
        let l2_chain_id = self.fetcher.l2_provider.get_chain_id().await?;

        if request.end_block - request.start_block > 1 && request.req_type == RequestType::Range {
            let failed_requests = self
                .db_client
                .fetch_failed_requests_by_block_range(
                    request.start_block,
                    request.end_block,
                    request.l1_chain_id as i64,
                    request.l2_chain_id as i64,
                    &self.program_config.commitments,
                )
                .await?;

            // NOTE: The failed_requests check here can be removed in V5 once the only failures that occur are unexecutable requests.
            if failed_requests.len() > 2 || execution_status == ExecutionStatus::Unexecutable {
                info!("Splitting request into two: {:?}", request);
                let mid_block = (request.start_block + request.end_block) / 2;
                let new_requests = vec![
                    OPSuccinctRequest::create_range_request(
                        request.mode,
                        request.start_block,
                        mid_block,
                        self.program_config.commitments.range_vkey_commitment.into(),
                        self.program_config.commitments.rollup_config_hash.into(),
                        l1_chain_id as i64,
                        l2_chain_id as i64,
                        self.fetcher.clone(),
                    )
                    .await?,
                    OPSuccinctRequest::create_range_request(
                        request.mode,
                        mid_block,
                        request.end_block,
                        self.program_config.commitments.range_vkey_commitment.into(),
                        self.program_config.commitments.rollup_config_hash.into(),
                        l1_chain_id as i64,
                        l2_chain_id as i64,
                        self.fetcher.clone(),
                    )
                    .await?,
                ];

                self.db_client.insert_requests(&new_requests).await?;
                return Ok(());
            }
        }

        // Retry the same request if splitting was not triggered.
        let mut new_request = request.clone();
        new_request.id = 0;
        new_request.status = RequestStatus::Unrequested;
        new_request.created_at = Local::now().naive_local();
        new_request.updated_at = Local::now().naive_local();
        new_request.proof_request_id = None;
        new_request.proof_request_time = None;
        new_request.checkpointed_l1_block_number = None;
        new_request.checkpointed_l1_block_hash = None;
        new_request.execution_statistics = serde_json::Value::Null;
        new_request.witnessgen_duration = None;
        new_request.execution_duration = None;
        new_request.prove_duration = None;
        new_request.relay_tx_hash = None;
        new_request.proof = None;

        self.db_client.insert_request(&new_request).await?;

        Ok(())
    }

    /// Makes a proof request by updating statuses, generating witnesses,
    /// and then either requesting or mocking the proof depending on configuration.
    #[tracing::instrument(name = "proof_requester.make_proof_request", skip(self, request))]
    pub async fn make_proof_request(&self, request: OPSuccinctRequest) -> Result<()> {
        // Update status to WitnessGeneration.
        self.db_client
            .update_request_status(request.id, RequestStatus::WitnessGeneration)
            .await?;

        info!(
            request_id = request.id,
            request_type = ?request.req_type,
            start_block = request.start_block,
            end_block = request.end_block,
            "Starting witness generation"
        );

        let witnessgen_duration = Instant::now();
        // Generate the stdin needed for the proof.
        let stdin = match request.req_type {
            RequestType::Range => self.range_proof_witnessgen(&request).await?,
            RequestType::Aggregation => {
                // Fetch consecutive range proofs from the database.
                let range_proofs_raw = self
                    .db_client
                    .get_consecutive_range_proofs(
                        request.start_block,
                        request.end_block,
                        &self.program_config.commitments,
                        request.l1_chain_id as i64,
                        request.l2_chain_id as i64,
                    )
                    .await?;
                let range_proofs: Vec<SP1ProofWithPublicValues> = range_proofs_raw
                    .iter()
                    .map(|proof| {
                        bincode::deserialize(proof.proof.as_ref().unwrap())
                            .expect("Deserialization failure for range proof")
                    })
                    .collect();
                self.agg_proof_witnessgen(&request, range_proofs).await?
            }
        };
        let duration = witnessgen_duration.elapsed();

        self.db_client
            .update_witnessgen_duration(request.id, duration.as_secs() as i64)
            .await?;

        info!(
            request_id = request.id,
            start_block = request.start_block,
            end_block = request.end_block,
            request_type = ?request.req_type,
            duration_s = duration.as_secs(),
            "Completed witness generation"
        );

        // For mock mode, update status to EXECUTION before proceeding.
        if self.mock {
            self.db_client
                .update_request_status(request.id, RequestStatus::Execution)
                .await?;
        }

        match request.req_type {
            RequestType::Range => {
                if self.mock {
                    let proof = match self.request_mock_range_proof(&request, stdin).await {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Failed to generate mock range proof: {}", e);
                            self.retry_request(
                                &request,
                                ExecutionStatus::UnspecifiedExecutionStatus,
                            )
                            .await?;
                            return Ok(());
                        }
                    };
                    let proof_bytes = bincode::serialize(&proof).unwrap();
                    self.db_client
                        .update_proof_to_complete(request.id, &proof_bytes)
                        .await?;
                } else {
                    let proof_id = self.request_range_proof(stdin).await?;
                    self.db_client
                        .update_request_to_prove(request.id, proof_id.into())
                        .await?;
                }
            }
            RequestType::Aggregation => {
                if self.mock {
                    let proof = match self.request_mock_agg_proof(stdin).await {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Failed to generate mock aggregation proof: {}", e);
                            self.retry_request(
                                &request,
                                ExecutionStatus::UnspecifiedExecutionStatus,
                            )
                            .await?;
                            return Ok(());
                        }
                    };
                    self.db_client
                        .update_proof_to_complete(request.id, &proof.bytes())
                        .await?;
                } else {
                    let proof_id = self.request_agg_proof(stdin).await?;
                    self.db_client
                        .update_request_to_prove(request.id, proof_id.into())
                        .await?;
                }
            }
        }

        Ok(())
    }
}
