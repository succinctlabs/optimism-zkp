use alloy_primitives::{Address, B256};
use anyhow::Result;
use op_succinct_host_utils::fetcher::OPSuccinctDataFetcher;
use op_succinct_host_utils::OPSuccinctL2OutputOracle;

/// Validate the config of the L2 output oracle matches the expected values.
pub async fn validate_config(
    address: Address,
    expected_agg_vkey: B256,
    expected_range_vkey: B256,
    expected_rollup_config_hash: B256,
    fetcher: &OPSuccinctDataFetcher,
) -> Result<bool> {
    let l2_output_oracle = OPSuccinctL2OutputOracle::new(address, fetcher.l1_provider.clone());

    let agg_vkey = l2_output_oracle.aggregationVkey().call().await?;
    let range_vkey = l2_output_oracle.rangeVkeyCommitment().call().await?;
    let rollup_config_hash = l2_output_oracle.rollupConfigHash().call().await?;

    let agg_vkey_valid = agg_vkey.aggregationVkey == expected_agg_vkey;
    let range_vkey_valid = range_vkey.rangeVkeyCommitment == expected_range_vkey;
    let rollup_config_hash_valid =
        rollup_config_hash.rollupConfigHash == expected_rollup_config_hash;

    Ok(agg_vkey_valid && range_vkey_valid && rollup_config_hash_valid)
}

/// Get the latest proposed block number from the L2 output oracle.
pub async fn get_latest_proposed_block_number(
    address: Address,
    fetcher: &OPSuccinctDataFetcher,
) -> Result<u64> {
    let l2_output_oracle = OPSuccinctL2OutputOracle::new(address, fetcher.l1_provider.clone());
    let block_number = l2_output_oracle.latestBlockNumber().call().await?;

    // Convert the block number to a u64.
    let block_number = block_number._0.try_into().unwrap();
    Ok(block_number)
}
