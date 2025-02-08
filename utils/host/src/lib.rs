pub mod block_range;
pub mod fetcher;
pub mod rollup_config;
pub mod stats;

use alloy_consensus::Header;
use alloy_primitives::B256;
use alloy_sol_types::sol;
use anyhow::anyhow;
use anyhow::Result;
use kona_host::eth::http_provider;
use kona_host::single::SingleChainHintHandler;
use kona_host::single::SingleChainHost;
use kona_host::single::SingleChainLocalInputs;
use kona_host::single::SingleChainProviders;
use kona_host::DiskKeyValueStore;
use kona_host::MemoryKeyValueStore;
use kona_host::OnlineHostBackend;
use kona_host::PreimageServer;
use kona_host::SharedKeyValueStore;
use kona_host::SplitKeyValueStore;
use kona_preimage::BidirectionalChannel;
use kona_preimage::Channel;
use kona_preimage::HintReader;
use kona_preimage::OracleServer;
use kona_providers_alloy::OnlineBeaconClient;
use kona_providers_alloy::OnlineBlobProvider;
use log::info;
use op_alloy_network::Optimism;
use op_succinct_client_utils::client::run_witnessgen_client;
use op_succinct_client_utils::InMemoryOracle;
use op_succinct_client_utils::{boot::BootInfoStruct, types::AggregationInputs};
use rkyv::to_bytes;
use sp1_sdk::{HashableKey, SP1Proof, SP1Stdin};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task;
use tokio::task::JoinHandle;

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract L2OutputOracle {
        bytes32 public aggregationVkey;
        bytes32 public rangeVkeyCommitment;
        bytes32 public rollupConfigHash;

        function updateAggregationVKey(bytes32 _aggregationVKey) external onlyOwner;

        function updateRangeVkeyCommitment(bytes32 _rangeVkeyCommitment) external onlyOwner;
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ProgramType {
    Single,
    Multi,
}

sol! {
    struct L2Output {
        uint64 zero;
        bytes32 l2_state_root;
        bytes32 l2_storage_hash;
        bytes32 l2_claim_hash;
    }
}

pub struct OPSuccinctHost {
    pub kona_args: SingleChainHost,
}

/// Get the stdin to generate a proof for the given L2 claim.
pub fn get_proof_stdin(oracle: InMemoryOracle) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::new();

    // Serialize the underlying KV store.
    let buffer = to_bytes::<rkyv::rancor::Error>(&oracle)?;

    let kv_store_bytes = buffer.into_vec();
    stdin.write_slice(&kv_store_bytes);

    Ok(stdin)
}

/// Get the stdin for the aggregation proof.
pub fn get_agg_proof_stdin(
    proofs: Vec<SP1Proof>,
    boot_infos: Vec<BootInfoStruct>,
    headers: Vec<Header>,
    multi_block_vkey: &sp1_sdk::SP1VerifyingKey,
    latest_checkpoint_head: B256,
) -> Result<SP1Stdin> {
    let mut stdin = SP1Stdin::new();
    for proof in proofs {
        let SP1Proof::Compressed(compressed_proof) = proof else {
            panic!();
        };
        stdin.write_proof(*compressed_proof, multi_block_vkey.vk.clone());
    }

    // Write the aggregation inputs to the stdin.
    stdin.write(&AggregationInputs {
        boot_infos,
        latest_l1_checkpoint_head: latest_checkpoint_head,
        multi_block_vkey: multi_block_vkey.hash_u32(),
    });
    // The headers have issues serializing with bincode, so use serde_json instead.
    let headers_bytes = serde_cbor::to_vec(&headers).unwrap();
    stdin.write_vec(headers_bytes);

    Ok(stdin)
}

/// Start the server and native client. Each server is tied to a single client.
/// TODO: Create your own host.
pub async fn start_server_and_native_client(
    cfg: SingleChainHost,
) -> Result<InMemoryOracle, anyhow::Error> {
    let host = OPSuccinctHost { kona_args: cfg };

    info!("Starting preimage server and client program.");
    let in_memory_oracle = host.run().await?;

    Ok(in_memory_oracle)
}

impl OPSuccinctHost {
    /// Run the host and client program.
    ///
    /// Returns the in-memory oracle which can be supplied to the zkVM.
    pub async fn run(&self) -> Result<InMemoryOracle> {
        let hint = BidirectionalChannel::new()?;
        let preimage = BidirectionalChannel::new()?;

        let server_task = self
            .kona_args
            .start_server(hint.host, preimage.host)
            .await?;

        let in_memory_oracle = run_witnessgen_client(preimage.client, hint.client).await?;
        // Unlike the upstream, manually abort the server task, as it will hang if you wait for both tasks to complete.
        server_task.abort();

        Ok(in_memory_oracle)
    }
}
