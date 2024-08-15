use alloy_primitives::hex;
use axum::{
    extract::{DefaultBodyLimit, Path},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose, Engine as _};
use client_utils::{RawBootInfo, BOOT_INFO_SIZE};
use host_utils::{fetcher::SP1KonaDataFetcher, get_agg_proof_stdin, get_proof_stdin, ProgramType};
use log::{error, info};
use serde::{Deserialize, Deserializer, Serialize};
use sp1_sdk::{
    network::client::NetworkClient,
    proto::network::{ProofMode, ProofStatus as SP1ProofStatus},
    utils, NetworkProver, Prover, SP1Proof, SP1ProofWithPublicValues,
};
use std::{env, fs, process::Command, time::Duration};
use tower_http::limit::RequestBodyLimitLayer;
use zkvm_host::{convert_host_cli_to_args, utils::fetch_header_preimages};

pub const MULTI_BLOCK_ELF: &[u8] = include_bytes!("../../elf/validity-client-elf");
pub const AGG_ELF: &[u8] = include_bytes!("../../elf/aggregation-client-elf");

#[derive(Deserialize, Serialize, Debug)]
struct SpanProofRequest {
    start: u64,
    end: u64,
}

#[derive(Deserialize, Serialize, Debug)]
struct AggProofRequest {
    #[serde(deserialize_with = "deserialize_base64_vec")]
    subproofs: Vec<Vec<u8>>,
    head: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct ProofResponse {
    proof_id: String,
}

#[derive(Serialize)]
struct ProofStatus {
    status: String,
    proof: Vec<u8>,
}

#[tokio::main]
async fn main() {
    utils::setup_logger();

    let app = Router::new()
        .route("/request_span_proof", post(request_span_proof))
        .route("/request_agg_proof", post(request_agg_proof))
        .route("/status/:proof_id", get(get_proof_status))
        .layer(DefaultBodyLimit::disable())
        .layer(RequestBodyLimitLayer::new(102400 * 1024 * 1024));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();

    info!("Server listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn request_span_proof(
    Json(payload): Json<SpanProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received span proof request: {:?}", payload);
    dotenv::dotenv().ok();
    // ZTODO: Save data fetcher, NetworkProver, and NetworkClient globally
    // and access via Store.
    let data_fetcher = SP1KonaDataFetcher::new();

    let host_cli = data_fetcher
        .get_host_cli_args(payload.start, payload.end, ProgramType::Multi)
        .await?;

    let data_dir = host_cli.data_dir.clone().unwrap();

    // Overwrite existing data directory.
    fs::create_dir_all(&data_dir)?;

    // Start the server and native client with a timeout
    // TODO: This is a heavy process and should be handled in the background.
    let metadata = cargo_metadata::MetadataCommand::new()
        .exec()
        .expect("Failed to get cargo metadata");
    let target_dir = metadata.target_directory.join("release");

    // Start the native host runner with a timeout.
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(40),
        tokio::process::Command::new(target_dir.join("native_host_runner"))
            .args(convert_host_cli_to_args(&host_cli))
            .env("RUST_LOG", "info")
            .spawn()?
            .wait(),
    )
    .await;

    match result {
        Ok(status) => status?,
        Err(_) => {
            error!("Native host runner process timed out after 30 seconds");
            return Err(AppError(anyhow::anyhow!(
                "Native host runner process timed out after 30 seconds"
            )));
        }
    };

    let sp1_stdin = get_proof_stdin(&host_cli)?;

    let prover = NetworkProver::new();
    let proof_id = prover
        .request_proof(MULTI_BLOCK_ELF, sp1_stdin, ProofMode::Compressed)
        .await?;

    Ok((StatusCode::OK, Json(ProofResponse { proof_id })))
}

async fn request_agg_proof(
    Json(payload): Json<AggProofRequest>,
) -> Result<(StatusCode, Json<ProofResponse>), AppError> {
    info!("Received agg proof request");
    let mut proofs_with_pv: Vec<SP1ProofWithPublicValues> = payload
        .subproofs
        .iter()
        .map(|sp| bincode::deserialize(sp).unwrap())
        .collect();

    let boot_infos: Vec<RawBootInfo> = proofs_with_pv
        .iter_mut()
        .map(|proof| {
            let mut boot_info_buf = [0u8; BOOT_INFO_SIZE];
            proof.public_values.read_slice(&mut boot_info_buf);
            RawBootInfo::abi_decode(&boot_info_buf).unwrap()
        })
        .collect();

    let proofs: Vec<SP1Proof> = proofs_with_pv
        .iter_mut()
        .map(|proof| proof.proof.clone())
        .collect();

    // ZTODO: Better error handling.
    let l1_head_bytes = hex::decode(payload.head.strip_prefix("0x").unwrap())?;
    let l1_head: [u8; 32] = l1_head_bytes.try_into().unwrap();

    let headers = fetch_header_preimages(&boot_infos, l1_head.into()).await?;

    let prover = NetworkProver::new();
    let (_, vkey) = prover.setup(MULTI_BLOCK_ELF);

    let stdin = get_agg_proof_stdin(proofs, boot_infos, headers, &vkey, l1_head.into()).unwrap();
    let proof_id = prover
        .request_proof(AGG_ELF, stdin, ProofMode::Plonk)
        .await?;

    Ok((StatusCode::OK, Json(ProofResponse { proof_id })))
}

async fn get_proof_status(
    Path(proof_id): Path<String>,
) -> Result<(StatusCode, Json<ProofStatus>), AppError> {
    info!("Received proof status request: {:?}", proof_id);
    dotenv::dotenv().ok();
    let private_key = env::var("SP1_PRIVATE_KEY")?;

    let client = NetworkClient::new(&private_key);

    // Time out this request if it takes too long.
    let timeout = Duration::from_secs(10);
    let (status, maybe_proof) = tokio::time::timeout(timeout, client.get_proof_status(&proof_id))
        .await
        .map_err(|_| AppError(anyhow::anyhow!("Proof status request timed out")))?
        .map_err(|e| AppError(anyhow::anyhow!("Failed to get proof status: {}", e)))?;

    let status: SP1ProofStatus = SP1ProofStatus::try_from(status.status)?;
    if status == SP1ProofStatus::ProofFulfilled {
        let proof: SP1ProofWithPublicValues = maybe_proof.unwrap();

        match proof.proof.clone() {
            SP1Proof::Compressed(_) => {
                // If it's a compressed proof, we need to serialize the entire struct with bincode.
                // Note: We're re-serializing the entire struct with bincode here, but this is fine
                // because we're on localhost and the size of the struct is small.
                let proof_bytes = bincode::serialize(&proof).unwrap();
                return Ok((
                    StatusCode::OK,
                    Json(ProofStatus {
                        status: status.as_str_name().to_string(),
                        proof: proof_bytes,
                    }),
                ));
            }
            SP1Proof::Plonk(_) => {
                // If it's a PLONK proof, we need to get the proof bytes that we put on-chain.
                let proof_bytes = proof.bytes();
                return Ok((
                    StatusCode::OK,
                    Json(ProofStatus {
                        status: status.as_str_name().to_string(),
                        proof: proof_bytes,
                    }),
                ));
            }
            _ => (),
        }
    }
    Ok((
        StatusCode::OK,
        Json(ProofStatus {
            status: status.as_str_name().to_string(),
            proof: vec![],
        }),
    ))
}

pub struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", self.0)).into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

/// Deserialize a vector of base64 strings into a vector of vectors of bytes. Go serializes
/// the subproofs as base64 strings.
fn deserialize_base64_vec<'de, D>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    let s: Vec<String> = Deserialize::deserialize(deserializer)?;
    s.into_iter()
        .map(|base64_str| {
            general_purpose::STANDARD
                .decode(base64_str)
                .map_err(serde::de::Error::custom)
        })
        .collect()
}
