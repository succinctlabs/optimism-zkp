use std::{env, sync::Arc};

use alloy_primitives::Address;
use anyhow::Result;
use sp1_sdk::network::FulfillmentStrategy;

use crate::{DriverDBClient, RequesterConfig};

pub async fn read_env(
    l1_chain_id: i64,
    l2_chain_id: i64,
) -> Result<(Arc<DriverDBClient>, RequesterConfig)> {
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL is not set");
    let db_client = Arc::new(DriverDBClient::new(&db_url).await?);

    let range_proof_strategy = if env::var("RANGE_PROOF_STRATEGY")
        .unwrap_or_else(|_| "reserved".to_string())
        .to_lowercase()
        == "hosted"
    {
        FulfillmentStrategy::Hosted
    } else {
        FulfillmentStrategy::Reserved
    };

    let agg_proof_strategy = if env::var("AGG_PROOF_STRATEGY")
        .unwrap_or_else(|_| "reserved".to_string())
        .to_lowercase()
        == "hosted"
    {
        FulfillmentStrategy::Hosted
    } else {
        FulfillmentStrategy::Reserved
    };

    let agg_proof_mode = if env::var("AGG_PROOF_MODE")
        .unwrap_or_else(|_| "groth16".to_string())
        .to_lowercase()
        == "plonk"
    {
        sp1_sdk::SP1ProofMode::Plonk
    } else {
        sp1_sdk::SP1ProofMode::Groth16
    };

    let l2oo_address = env::var("L2OO_ADDRESS")
        .expect("L2OO_ADDRESS is not set")
        .parse()
        .expect("Invalid L2OO_ADDRESS");

    let requester_config = RequesterConfig {
        l1_chain_id,
        l2_chain_id,
        l2oo_address,
        dgf_address: Address::ZERO,
        range_proof_interval: env::var("RANGE_PROOF_INTERVAL")
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .expect("Invalid RANGE_PROOF_INTERVAL"),
        max_concurrent_witness_gen: env::var("MAX_CONCURRENT_WITNESS_GEN")
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .expect("Invalid MAX_CONCURRENT_WITNESS_GEN"),
        max_concurrent_proof_requests: env::var("MAX_CONCURRENT_PROOF_REQUESTS")
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .expect("Invalid MAX_CONCURRENT_PROOF_REQUESTS"),
        submission_interval: env::var("SUBMISSION_INTERVAL")
            .unwrap_or_else(|_| "10".to_string())
            .parse()
            .expect("Invalid SUBMISSION_INTERVAL"),
        range_proof_strategy,
        agg_proof_strategy,
        agg_proof_mode,
        mock: env::var("OP_SUCCINCT_MOCK")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .expect("Invalid OP_SUCCINCT_MOCK"),
    };

    Ok((db_client, requester_config))
}
