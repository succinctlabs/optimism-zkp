use alloy_primitives::{Address, B256};
use anyhow::Result;
use chrono::{Local, NaiveDateTime};
use op_succinct_host_utils::fetcher::{BlockInfo, OPSuccinctDataFetcher};
use serde_json::Value;
use sqlx::types::BigDecimal;
use sqlx::Error;
use sqlx::{postgres::PgQueryResult, FromRow, PgPool};
use std::fmt::Debug;
use std::sync::Arc;

use crate::CommitmentConfig;

#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
#[repr(i16)]
pub enum RequestStatus {
    #[default]
    Unrequested = 0,
    WitnessGeneration = 1,
    Execution = 2,
    Prove = 3,
    Complete = 4,
    Relayed = 5,
    Failed = 6,
    Cancelled = 7,
}

impl From<i16> for RequestStatus {
    fn from(value: i16) -> Self {
        match value {
            0 => RequestStatus::Unrequested,
            1 => RequestStatus::WitnessGeneration,
            2 => RequestStatus::Execution,
            3 => RequestStatus::Prove,
            4 => RequestStatus::Complete,
            5 => RequestStatus::Relayed,
            6 => RequestStatus::Failed,
            7 => RequestStatus::Cancelled,
            _ => panic!("Invalid request status: {}", value),
        }
    }
}


#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
pub enum RequestType {
    #[default]
    Range = 0,
    Aggregation = 1,
}

impl From<i16> for RequestType {
    fn from(value: i16) -> Self {
        match value {
            0 => RequestType::Range,
            1 => RequestType::Aggregation,
            _ => panic!("Invalid request type: {}", value),
        }
    }
}

#[derive(sqlx::Type, Debug, Copy, Clone, PartialEq, Eq, Default)]
#[sqlx(type_name = "smallint")]
pub enum RequestMode {
    #[default]
    Real = 0,
    Mock = 1,
}

impl From<i16> for RequestMode {
    fn from(value: i16) -> Self {
        match value {
            0 => RequestMode::Real,
            1 => RequestMode::Mock,
            _ => panic!("Invalid request mode: {}", value),
        }
    }
}

#[derive(FromRow, Default, Clone)]
pub struct OPSuccinctRequest {
    pub id: i64,
    pub status: RequestStatus,
    pub req_type: RequestType,
    pub mode: RequestMode,
    pub start_block: i64,
    pub end_block: i64,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub proof_request_id: Option<Vec<u8>>, //B256
    pub proof_request_time: Option<NaiveDateTime>,
    pub checkpointed_l1_block_number: Option<i64>,
    pub checkpointed_l1_block_hash: Option<Vec<u8>>, //B256
    pub execution_statistics: Value,
    pub witnessgen_duration: Option<i64>,
    pub execution_duration: Option<i64>,
    pub prove_duration: Option<i64>,
    pub range_vkey_commitment: Vec<u8>, //B256
    pub aggregation_vkey_hash: Option<Vec<u8>>, //B256
    pub rollup_config_hash: Vec<u8>, //B256
    pub relay_tx_hash: Option<Vec<u8>>, //B256
    pub proof: Option<Vec<u8>>, // Bytes
    pub total_nb_transactions: i64,
    pub total_eth_gas_used: i64,
    pub total_l1_fees: BigDecimal,
    pub total_tx_fees: BigDecimal,
    pub l1_chain_id: i64,
    pub l2_chain_id: i64,
    pub contract_address: Option<Vec<u8>>, //Address
}

impl Debug for OPSuccinctRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OPSuccinctRequest {{ id: {}, status: {:?}, req_type: {:?}, mode: {:?}, start_block: {}, end_block: {}, created_at: {}, updated_at: {}, proof_request_id: {:?}, proof_request_time: {:?}, checkpointed_l1_block_number: {:?}, checkpointed_l1_block_hash: {:?}, execution_statistics: {}, witnessgen_duration: {:?}, execution_duration: {:?}, prove_duration: {:?}, range_vkey_commitment: {}, aggregation_vkey_hash: {:?}, rollup_config_hash: {}, relay_tx_hash: {:?}, proof: {:?}, total_nb_transactions: {}, total_eth_gas_used: {}, total_l1_fees: {}, total_tx_fees: {}, l1_chain_id: {}, l2_chain_id: {}, contract_address: {:?} }}", 
            self.id,
            self.status,
            self.req_type, 
            self.mode,
            self.start_block,
            self.end_block,
            self.created_at,
            self.updated_at,
            self.proof_request_id.as_ref().map(|id| B256::from_slice(id)),
            self.proof_request_time,
            self.checkpointed_l1_block_number,
            self.checkpointed_l1_block_hash.as_ref().map(|hash| B256::from_slice(hash)),
            self.execution_statistics,
            self.witnessgen_duration,
            self.execution_duration,
            self.prove_duration,
            B256::from_slice(&self.range_vkey_commitment),
            self.aggregation_vkey_hash.as_ref().map(|hash| B256::from_slice(hash)),
            B256::from_slice(&self.rollup_config_hash),
            self.relay_tx_hash.as_ref().map(|hash| B256::from_slice(hash)),
            self.proof.as_ref().map(|p| format!("[{} bytes]", p.len())),
            self.total_nb_transactions,
            self.total_eth_gas_used,
            self.total_l1_fees,
            self.total_tx_fees,
            self.l1_chain_id,
            self.l2_chain_id,
            self.contract_address.as_ref().map(|addr| Address::from_slice(addr)),
        )
    }
}

impl OPSuccinctRequest {
    /// Creates a new range request and fetches the block data.
    pub async fn create_range_request(
        mode: RequestMode,
        start_block: i64,
        end_block: i64,
        range_vkey_commitment: B256,
        rollup_config_hash: B256,
        l1_chain_id: i64,
        l2_chain_id: i64,
        fetcher: Arc<OPSuccinctDataFetcher>,
    ) -> Result<Self> {
        let block_data = fetcher.get_l2_block_data_range(start_block as u64, end_block as u64).await?;

        Ok(Self::new_range_request(mode, start_block, end_block, range_vkey_commitment, rollup_config_hash, block_data, l1_chain_id, l2_chain_id))
    }

    /// Create a new range request given the block data.
    pub fn new_range_request(
        mode: RequestMode,
        start_block: i64,
        end_block: i64,
        range_vkey_commitment: B256,
        rollup_config_hash: B256,
        block_data: Vec<BlockInfo>,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Self {
        let now = Local::now().naive_local();

        let total_nb_transactions: u64 = block_data.iter().map(|b| b.transaction_count).sum();
        let total_eth_gas_used: u64 = block_data.iter().map(|b| b.gas_used).sum();
        // Note: The transaction fees are a superset of the L1 fees.
        let total_l1_fees: u128 = block_data.iter().map(|b| b.total_l1_fees).sum();
        let total_tx_fees: u128 = block_data.iter().map(|b| b.total_tx_fees).sum();
        
        Self {
            id: 0,
            status: RequestStatus::Unrequested,
            req_type: RequestType::Range,
            mode,
            start_block,
            end_block,
            created_at: now,
            updated_at: now,
            range_vkey_commitment: range_vkey_commitment.to_vec(),
            rollup_config_hash: rollup_config_hash.to_vec(),
            total_nb_transactions: total_nb_transactions as i64,
            total_eth_gas_used: total_eth_gas_used as i64,
            total_l1_fees: total_l1_fees.into(),
            total_tx_fees: total_tx_fees.into(),
            l1_chain_id,
            l2_chain_id,
            ..Default::default()
        }
    }

    /// Create a new aggregation request.
    pub fn new_agg_request(
        mode: RequestMode,
        start_block: i64,
        end_block: i64,
        range_vkey_commitment: B256,
        aggregation_vkey_hash: B256,
        rollup_config_hash: B256,
        l1_chain_id: i64,
        l2_chain_id: i64,
        checkpointed_l1_block_number: i64,
        checkpointed_l1_block_hash: B256,
    ) -> Self {
        let now = Local::now().naive_local();

        Self {
            id: 0,
            status: RequestStatus::Unrequested,
            req_type: RequestType::Aggregation,
            mode,
            start_block,
            end_block,
            created_at: now,
            updated_at: now,
            checkpointed_l1_block_number: Some(checkpointed_l1_block_number),
            checkpointed_l1_block_hash: Some(checkpointed_l1_block_hash.to_vec()),
            range_vkey_commitment: range_vkey_commitment.to_vec(),
            aggregation_vkey_hash: Some(aggregation_vkey_hash.to_vec()),
            rollup_config_hash: rollup_config_hash.to_vec(),
            l1_chain_id,
            l2_chain_id,
            ..Default::default()
        }
    }
}

#[derive(FromRow, Debug)]
pub struct EthMetrics {
    pub id: i64,
    pub block_nb: i64,
    pub nb_transactions: i64,
    pub eth_gas_used: i64,
    // sqlx doesn't support u128, use BigDecimal instead for fees.
    // Fees are in wei, can be greater than 9 * 10^18.
    pub l1_fees: BigDecimal,
    pub tx_fees: BigDecimal,
}

pub struct DriverDBClient {
    pub pool: PgPool,
}

impl DriverDBClient {
    pub async fn new(database_url: &str) -> Result<Self> {
        let pool = PgPool::connect(database_url).await?;

        // Run migrations.
        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(DriverDBClient { pool })
    }

    pub async fn insert_request(&self, req: &OPSuccinctRequest) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            INSERT INTO requests (
                status,
                req_type,
                mode,
                start_block,
                end_block,
                created_at,
                updated_at,
                proof_request_id,
                proof_request_time,
                checkpointed_l1_block_number,
                checkpointed_l1_block_hash,
                execution_statistics,
                witnessgen_duration,
                execution_duration,
                prove_duration,
                range_vkey_commitment,
                aggregation_vkey_hash,
                rollup_config_hash,
                relay_tx_hash,
                proof,
                total_nb_transactions,
                total_eth_gas_used,
                total_l1_fees,
                total_tx_fees,
                l1_chain_id,
                l2_chain_id,
                contract_address
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27
            )
            "#,
            req.status as i16,
            req.req_type as i16,
            req.mode as i16,
            req.start_block as i64,
            req.end_block as i64,
            req.created_at,
            req.updated_at,
            req.proof_request_id.as_ref().map(|arr| &arr[..]),
            req.proof_request_time,
            req.checkpointed_l1_block_number.map(|n| n as i64),
            req.checkpointed_l1_block_hash.as_ref().map(|arr| &arr[..]),
            req.execution_statistics,
            // Storing durations in seconds.
            req.witnessgen_duration,
            req.execution_duration,
            req.prove_duration,
            &req.range_vkey_commitment,
            req.aggregation_vkey_hash.as_ref().map(|arr| &arr[..]),
            &req.rollup_config_hash,
            req.relay_tx_hash.as_ref().map(|arr| &arr[..]),
            req.proof.as_ref().map(|arr| &arr[..]),
            req.total_nb_transactions,
            req.total_eth_gas_used,
            req.total_l1_fees.clone(),
            req.total_tx_fees.clone(),
            req.l1_chain_id,
            req.l2_chain_id,
            req.contract_address.as_ref().map(|arr| &arr[..]),
        )
        .execute(&self.pool)
        .await
    }

    pub async fn insert_eth_metrics(&self, metrics: &EthMetrics) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            INSERT INTO eth_metrics (
                block_nb,
                nb_transactions,
                eth_gas_used,
                l1_fees,
                tx_fees
            ) VALUES ($1, $2, $3, $4, $5)
            "#,
            metrics.block_nb,
            metrics.nb_transactions,
            metrics.eth_gas_used,
            metrics.l1_fees,
            metrics.tx_fees,
        )
        .execute(&self.pool)
        .await
    }

    /// Fetch all requests from the database.
    pub async fn fetch_requests(&self) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Fetch all Ethereum metrics from the database.
    pub async fn fetch_eth_metrics(&self) -> Result<Vec<EthMetrics>, Error> {
        let metrics = sqlx::query_as!(
            EthMetrics,
            "SELECT * FROM eth_metrics"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(metrics)
    }

    /// Fetch all requests with a specific block range and status FAILED or CANCELLED.
    ///
    /// Checks that the request has the same range vkey commitment and rollup config hash as the commitment.
    pub async fn fetch_failed_requests_by_block_range(
        &self,
        start_block: i64,
        end_block: i64,
        l1_chain_id: i64,
        l2_chain_id: i64,
        commitment: &CommitmentConfig,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE start_block = $1 AND end_block = $2 AND (status = $3 OR status = $4) AND range_vkey_commitment = $5 AND rollup_config_hash = $6 AND l1_chain_id = $7 AND l2_chain_id = $8",
            start_block as i64,
            end_block as i64,
            RequestStatus::Failed as i16,
            RequestStatus::Cancelled as i16,
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Fetch the highest block number of a request with one of the given statuses and commitment.
    pub async fn fetch_highest_range_request_with_statuses_and_commitment(
        &self,
        statuses: &[RequestStatus],
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = ANY($3) AND req_type = $4 AND l1_chain_id = $5 AND l2_chain_id = $6 ORDER BY start_block DESC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &status_values[..],
            RequestType::Range as i16,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    /// Fetch all range requests that have one of the given statuses and start block >= latest_contract_l2_block.
    pub async fn fetch_range_requests_with_status_and_start_block(
        &self,
        statuses: &[RequestStatus],
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        let requests = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = ANY($3) AND req_type = $4 AND start_block >= $5 AND l1_chain_id = $6 AND l2_chain_id = $7",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &status_values[..],
            RequestType::Range as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Fetch all requests that have one of the given statuses.
    pub async fn fetch_requests_with_status(
        &self,
        statuses: &[RequestStatus],
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        let requests = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = ANY($3) AND l1_chain_id = $4 AND l2_chain_id = $5",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &status_values[..],
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Fetch all requests with a specific status from the database.
    pub async fn fetch_requests_by_status(
        &self,
        status: RequestStatus,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as::<_, OPSuccinctRequest>(
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND l1_chain_id = $4 AND l2_chain_id = $5"
        )
        .bind(&commitment.range_vkey_commitment[..])
        .bind(&commitment.rollup_config_hash[..])
        .bind(status as i16)
        .bind(l1_chain_id)
        .bind(l2_chain_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Get the consecutive range proofs for a given start block and end block that are complete with the same range vkey commitment.
    pub async fn get_consecutive_range_proofs(
        &self,
        start_block: i64,
        end_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND req_type = $4 AND start_block >= $5 AND end_block <= $6 AND l1_chain_id = $7 AND l2_chain_id = $8 ORDER BY start_block ASC",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            RequestStatus::Complete as i16,
            RequestType::Range as i16,
            start_block,
            end_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Fetch an aggregation request with the same start block, end block, and commitment config that has a checkpointed block hash.
    pub async fn fetch_agg_request_with_checkpointed_block_hash(
        &self,
        start_block: i64,
        end_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND req_type = $4 AND start_block = $5 AND end_block = $6 AND status = $7 AND checkpointed_l1_block_hash IS NOT NULL AND checkpointed_l1_block_number IS NOT NULL AND l1_chain_id = $8 AND l2_chain_id = $9 LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &commitment.agg_vkey_hash[..],
            RequestType::Aggregation as i16,
            start_block,
            end_block,
            RequestStatus::Failed as i16,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(request)
    }

    /// Fetch all non-failed Aggregation proofs with the same start block, range vkey commitment, and aggregation vkey.
    pub async fn fetch_active_agg_proofs(
        &self,
        start_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as::<_, OPSuccinctRequest>(
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND status != $4 AND status != $5 AND req_type = $6 AND start_block = $7 AND l1_chain_id = $8 AND l2_chain_id = $9 ORDER BY start_block ASC"
        )
        .bind(&commitment.range_vkey_commitment[..])
        .bind(&commitment.rollup_config_hash[..])
        .bind(&commitment.agg_vkey_hash[..])
        .bind(RequestStatus::Failed as i16)
        .bind(RequestStatus::Cancelled as i16)
        .bind(RequestType::Aggregation as i16)
        .bind(start_block)
        .bind(l1_chain_id)
        .bind(l2_chain_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Fetch the sorted list of Aggregation proofs with status Unrequested that have a start_block >= latest_contract_l2_block.
    ///
    /// Checks that the request has the same range vkey commitment, aggregation vkey, and rollup config hash as the commitment.
    ///
    /// NOTE: There should only be one "pending" Unrequested agg proof at a time for a specific start block.
    pub async fn fetch_unrequested_agg_proof(
        &self,
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND status = $4 AND req_type = $5 AND start_block >= $6 AND l1_chain_id = $7 AND l2_chain_id = $8 ORDER BY start_block ASC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &commitment.agg_vkey_hash[..],
            RequestStatus::Unrequested as i16,
            RequestType::Aggregation as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(request)
    }

    /// Fetch the sorted list of Range proofs with status Unrequested that have a start_block >= latest_contract_l2_block.
    pub async fn fetch_unrequested_range_proofs(
        &self,
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND req_type = $4 AND start_block >= $5 AND l1_chain_id = $6 AND l2_chain_id = $7 ORDER BY start_block ASC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            RequestStatus::Unrequested as i16,
            RequestType::Range as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    /// Fetch all completed range proofs with matching range_vkey_commitment and start_block >= latest_contract_l2_block
    pub async fn fetch_completed_range_proofs(
        &self,
        commitment: &CommitmentConfig,
        latest_contract_l2_block: i64,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Vec<OPSuccinctRequest>, Error> {
        let requests = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND status = $3 AND req_type = $4 AND start_block >= $5 AND l1_chain_id = $6 AND l2_chain_id = $7 ORDER BY start_block ASC",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            RequestStatus::Complete as i16,
            RequestType::Range as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(requests)
    }

    /// Update the prove_duration based on the current time and the proof_request_time.
    pub async fn update_prove_duration(
        &self,
        id: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET prove_duration = EXTRACT(EPOCH FROM (NOW() - proof_request_time))::BIGINT WHERE id = $1
            "#,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Add a completed proof to the database.
    pub async fn update_proof_to_complete(
        &self,
        id: i64,
        proof: &[u8],
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET proof = $1, status = $2, updated_at = NOW() WHERE id = $3
            "#,
            proof,
            RequestStatus::Complete as i16,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update the witness generation duration of a request in the database.
    pub async fn update_witnessgen_duration(
        &self,
        id: i64,
        duration: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET witnessgen_duration = $1, updated_at = NOW() WHERE id = $2
            "#,
            duration,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update the status of a request in the database.
    pub async fn update_request_status(
        &self,
        id: i64,
        new_status: RequestStatus,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests 
            SET status = $1, updated_at = NOW()
            WHERE id = $2
            "#,
            new_status as i16,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update the status of a request to Prove.
    /// 
    /// Updates the proof_request_time to the current time.
    pub async fn update_request_to_prove(
        &self,
        id: i64,
        proof_id: B256,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests 
            SET status = $1, proof_request_id = $2, proof_request_time = NOW(), updated_at = NOW()
            WHERE id = $3
            "#,
            RequestStatus::Prove as i16,
            proof_id.to_vec(),
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Update status of a request to RELAYED.
    pub async fn update_request_to_relayed(
        &self,
        id: i64,
        relay_tx_hash: B256,
        contract_address: Address,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET status = $1, relay_tx_hash = $2, contract_address = $3, updated_at = NOW() WHERE id = $4
            "#,
            RequestStatus::Relayed as i16,
            relay_tx_hash.to_vec(),
            contract_address.to_vec(),
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Fetch a single completed aggregation proof.
    pub async fn fetch_completed_aggregation_proofs(
        &self,
        latest_contract_l2_block: i64,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<Option<OPSuccinctRequest>, Error> {
        let request = sqlx::query_as!(
            OPSuccinctRequest,
            "SELECT * FROM requests WHERE range_vkey_commitment = $1 AND rollup_config_hash = $2 AND aggregation_vkey_hash = $3 AND status = $4 AND req_type = $5 AND start_block = $6 AND l1_chain_id = $7 AND l2_chain_id = $8 ORDER BY start_block ASC LIMIT 1",
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            &commitment.agg_vkey_hash[..],
            RequestStatus::Complete as i16,
            RequestType::Aggregation as i16,
            latest_contract_l2_block,
            l1_chain_id,
            l2_chain_id,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(request)
    }

    /// Insert the execution statistics for a request.
    pub async fn insert_execution_statistics(
        &self,
        id: i64,
        execution_statistics: Value,
        execution_duration_secs: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET execution_statistics = $1, execution_duration = $2 WHERE id = $3
            "#,
            &execution_statistics,
            execution_duration_secs,
            id,
        )
        .execute(&self.pool)
        .await
    }

    /// Cancel all requests with the given status
    pub async fn cancel_all_requests_with_statuses(
        &self,
        statuses: &[RequestStatus],
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        sqlx::query!(
            r#"
            UPDATE requests SET status = $1 WHERE status = ANY($2) AND l1_chain_id = $3 AND l2_chain_id = $4
            "#,
            RequestStatus::Cancelled as i16,
            &status_values[..],
            l1_chain_id,
            l2_chain_id,
        )
        .execute(&self.pool)
        .await
    }

    /// Drop all requests with the given statuses
    pub async fn delete_all_requests_with_statuses(
        &self,
        statuses: &[RequestStatus],
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        let status_values: Vec<i16> = statuses.iter().map(|s| *s as i16).collect();
        sqlx::query!(
            r#"
            DELETE FROM requests WHERE status = ANY($1) AND l1_chain_id = $2 AND l2_chain_id = $3
            "#,
            &status_values[..],
            l1_chain_id,
            l2_chain_id,
        )
        .execute(&self.pool)
        .await
    }

    /// Cancel all prove requests with a different commitment config and same chain id's
    pub async fn cancel_prove_requests_with_different_commitment_config(
        &self,
        commitment: &CommitmentConfig,
        l1_chain_id: i64,
        l2_chain_id: i64,
    ) -> Result<PgQueryResult, Error> {
        sqlx::query!(
            r#"
            UPDATE requests SET status = $1 WHERE status = $2 AND (range_vkey_commitment != $3 OR rollup_config_hash != $4) AND l1_chain_id = $5 AND l2_chain_id = $6
            "#,
            RequestStatus::Cancelled as i16,
            RequestStatus::Prove as i16,
            &commitment.range_vkey_commitment[..],
            &commitment.rollup_config_hash[..],
            l1_chain_id,
            l2_chain_id,
        )
        .execute(&self.pool)
        .await
    }

    /// Batch insert requests.
    pub async fn insert_requests(
        &self,
        requests: &[OPSuccinctRequest],
    ) -> Result<PgQueryResult, Error> {
        let mut query_builder = sqlx::QueryBuilder::new(
            "INSERT INTO requests (
                status, req_type, mode, start_block, end_block, created_at, updated_at,
                proof_request_id, proof_request_time, checkpointed_l1_block_number, checkpointed_l1_block_hash, execution_statistics,
                witnessgen_duration, execution_duration, prove_duration, range_vkey_commitment,
                aggregation_vkey_hash, rollup_config_hash, relay_tx_hash, proof, total_nb_transactions, total_eth_gas_used, total_l1_fees, total_tx_fees, l1_chain_id, l2_chain_id, contract_address) ",
        );

        query_builder.push_values(requests, |mut b, req| {
            b.push_bind(req.status as i16)
                .push_bind(req.req_type as i16)
                .push_bind(req.mode as i16)
                .push_bind(req.start_block)
                .push_bind(req.end_block)
                .push_bind(req.created_at)
                .push_bind(req.updated_at)
                .push_bind(req.proof_request_id.as_ref().map(|arr| &arr[..]))
                .push_bind(req.proof_request_time)
                .push_bind(req.checkpointed_l1_block_number)
                .push_bind(req.checkpointed_l1_block_hash.as_ref().map(|arr| &arr[..]))
                .push_bind(&req.execution_statistics)
                .push_bind(req.witnessgen_duration)
                .push_bind(req.execution_duration)
                .push_bind(req.prove_duration)
                .push_bind(&req.range_vkey_commitment[..])
                .push_bind(req.aggregation_vkey_hash.as_ref().map(|arr| &arr[..]))
                .push_bind(&req.rollup_config_hash[..])
                .push_bind(req.relay_tx_hash.as_ref())
                .push_bind(req.proof.as_ref())
                .push_bind(req.total_nb_transactions)
                .push_bind(req.total_eth_gas_used)
                .push_bind(req.total_l1_fees.clone())
                .push_bind(req.total_tx_fees.clone())
                .push_bind(req.l1_chain_id)
                .push_bind(req.l2_chain_id)
                .push_bind(req.contract_address.as_ref().map(|arr| &arr[..]));
        });

        query_builder.build().execute(&self.pool).await
    }
}
