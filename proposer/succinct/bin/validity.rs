use op_succinct_host_utils::fetcher::{OPSuccinctDataFetcher, RunContext};
use tracing::info;
use std::{env, sync::Arc};

use alloy_provider::{network::EthereumWallet, Provider, ProviderBuilder, WsConnect};
use alloy_signer_local::PrivateKeySigner;
use anyhow::Result;
use op_succinct_proposer::{read_env, OPChainMetricer, Proposer};

use tikv_jemallocator::Jemalloc;

#[global_allocator]
static ALLOCATOR: Jemalloc = Jemalloc;

use clap::Parser;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to environment file
    #[arg(long, default_value = ".env")]
    env_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    dotenv::from_filename(args.env_file).ok();

    // Set up logging using the provided format
    let format = tracing_subscriber::fmt::format()
        .with_level(true)
        .with_target(false)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_file(false)
        .with_line_number(false)
        .with_ansi(true);

    // Turn off all logging from kona.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into())
                .add_directive("single_hint_handler=error".parse().unwrap())
                .add_directive("execute=error".parse().unwrap())
                .add_directive("sp1_prover=error".parse().unwrap())
                .add_directive("boot-loader=error".parse().unwrap())
                .add_directive("client-executor=error".parse().unwrap())
                .add_directive("client=error".parse().unwrap())
                .add_directive("channel-assembler=error".parse().unwrap())
                .add_directive("attributes-queue=error".parse().unwrap())
                .add_directive("batch-validator=error".parse().unwrap())
                .add_directive("client-derivation-driver=error".parse().unwrap())
                .add_directive("host-server=error".parse().unwrap())
                .add_directive("kona_protocol=error".parse().unwrap())
                .add_directive("sp1_core_executor=off".parse().unwrap()),
        )
        .event_format(format)
        .init();

    let fetcher = OPSuccinctDataFetcher::new_with_rollup_config(RunContext::Dev).await?;

    // Read the environment variables.
    let (db_client, proposer_config) = read_env(
        fetcher.l1_provider.get_chain_id().await? as i64,
        fetcher.l2_provider.get_chain_id().await? as i64,
    )
    .await?;

    // Read all config from env vars
    let rpc_url = env::var("L1_RPC").expect("L1_RPC is not set");
    let private_key: PrivateKeySigner = env::var("PRIVATE_KEY")
        .expect("PRIVATE_KEY is not set")
        .parse()
        .expect("Failed to parse PRIVATE_KEY");
    let signer = EthereumWallet::new(private_key);
    let l1_provider = ProviderBuilder::new()
        .wallet(signer.clone())
        .on_http(rpc_url.parse().expect("Failed to parse L1_RPC"));

    let proposer = Proposer::new(
        l1_provider,
        db_client.clone(),
        Arc::new(fetcher),
        proposer_config,
    )
    .await?;

    info!("Initializing proposer");

    // let l2_ws_rpc = env::var("L2_WS_RPC").expect("L2_WS_RPC is not set");
    // let l2_provider = alloy_provider::ProviderBuilder::default()
    //     .on_ws(WsConnect::new(l2_ws_rpc))
    //     .await?;

    // // Create the OP chain metrics collector.
    // let eth_listener = OPChainMetricer::new(db_client.clone(), Arc::new(l2_provider));

    // // Spawn a thread for the ETH listener.
    // let eth_handle = tokio::spawn(async move {
    //     if let Err(e) = eth_listener.listen().await {
    //         tracing::error!("ETH listener error: {}", e);
    //         return Err(e);
    //     }
    //     Ok(())
    // });

    // Spawn a thread for the proposer.
    let proposer_handle = tokio::spawn(async move {
        if let Err(e) = proposer.run().await {
            tracing::error!("Proposer error: {}", e);
            return Err(e);
        }
        Ok(())
    });

    // // Spawn a thread for the metrics exporter.
    // info!("Initializing metrics exporter");
    // const METRICS_PORT: u16 = 7000;
    // op_succinct_proposer::init_metrics(&METRICS_PORT);

    // info!("Starting metrics update loop");

    // const METRICS_UPDATE_INTERVAL: u64 = 1;
    // let metrics_handle = tokio::spawn(async move {
    //     loop {
    //         op_succinct_proposer::update_cpu_and_memory();
    //         tokio::time::sleep(Duration::from_secs(METRICS_UPDATE_INTERVAL)).await;
    //     }
    // });

    // Wait for all tasks to complete.
    let proposer_res = proposer_handle.await?;
    if let Err(e) = proposer_res {
        tracing::error!("Proposer task failed: {}", e);
        return Err(e);
    }
    // let (eth_res, proposer_res) = tokio::try_join!(eth_handle, proposer_handle)?;
    // if let Err(e) = eth_res.or(proposer_res) {
    //     tracing::error!("Proposer task failed: {}", e);
    //     return Err(e);
    // }

    Ok(())
}
