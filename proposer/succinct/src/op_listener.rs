use alloy_provider::Provider;
use anyhow::Result;
use futures_util::StreamExt;
use op_alloy_network::Optimism;
use std::sync::Arc;

// Import our DB client and EthMetrics struct
use crate::db::{DriverDBClient, EthMetrics};

pub struct OPChainMetricer<P>
where
    P: Provider<Optimism> + 'static,
{
    db_client: Arc<DriverDBClient>,
    provider: Arc<P>,
}

impl<P> OPChainMetricer<P>
where
    P: Provider<Optimism> + 'static,
{
    pub fn new(db_client: Arc<DriverDBClient>, l2_provider: Arc<P>) -> Self {
        Self {
            db_client,
            provider: l2_provider,
        }
    }

    #[tracing::instrument(name = "op_listener", skip(self))]
    pub async fn listen(&self) -> Result<()> {
        let sub = self.provider.subscribe_blocks().await?;
        tracing::info!("Listening for new blocks on L2.");

        let mut stream = sub.into_stream();
        while let Some(header) = stream.next().await {
            let receipts = self
                .provider
                .get_block_receipts(header.number.into())
                .await?;
            if let Some(receipts) = receipts {
                tracing::debug!(
                    "Found {} receipts for block {}",
                    receipts.len(),
                    header.number
                );
                // Process fees and metrics calculation.
                let nb_transactions = receipts.len() as i64;
                let eth_gas_used = header.gas_used as i64;
                let mut tx_fees: u128 = 0;
                let mut l1_fees: u128 = 0;
                for receipt in receipts {
                    tx_fees += receipt.inner.effective_gas_price * receipt.inner.gas_used as u128;
                    if let Some(l1_fee) = receipt.l1_block_info.l1_fee {
                        l1_fees += l1_fee;
                    }
                }
                let metrics = EthMetrics {
                    id: 0,
                    block_nb: header.number as i64,
                    nb_transactions,
                    eth_gas_used,
                    l1_fees: l1_fees.into(),
                    tx_fees: tx_fees.into(),
                };
                match self.db_client.insert_eth_metrics(&metrics).await {
                    Ok(_) => tracing::debug!("Inserted metrics for block: {}", header.number),
                    Err(e) => tracing::error!(
                        "Error inserting metrics for block {}: {:?}",
                        header.number,
                        e
                    ),
                }
            }
        }
        Ok(())
    }
}
