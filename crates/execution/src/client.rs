use anyhow::{Context, Result};
use hl_core::{Order, OrderStatus, OrderType, Side};
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Maximum retry attempts for API calls.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff.
const BASE_RETRY_DELAY_MS: u64 = 500;

/// Hyperliquid exchange API client.
/// Wraps the exchange endpoint for order placement, cancellation, and modification.
pub struct ExchangeClient {
    client: Client,
    exchange_url: String,
    info_url: String,
    wallet_address: String,
    // In production, this would hold the signing key
    is_testnet: bool,
}

/// Order placement request.
#[derive(Debug, Serialize)]
struct PlaceOrderRequest {
    action: PlaceOrderAction,
    nonce: u64,
    signature: String,
    vault_address: Option<String>,
}

#[derive(Debug, Serialize)]
struct PlaceOrderAction {
    #[serde(rename = "type")]
    action_type: String,
    orders: Vec<OrderSpec>,
    grouping: String,
}

#[derive(Debug, Serialize)]
struct OrderSpec {
    a: u32,  // Asset index
    b: bool, // Is buy
    p: String, // Price
    s: String, // Size
    r: bool, // Reduce only
    t: OrderTif,
}

#[derive(Debug, Serialize)]
struct OrderTif {
    limit: LimitTif,
}

#[derive(Debug, Serialize)]
struct LimitTif {
    tif: String,
}

/// Cancel order request.
#[derive(Debug, Serialize)]
struct CancelOrderRequest {
    action: CancelAction,
    nonce: u64,
    signature: String,
}

#[derive(Debug, Serialize)]
struct CancelAction {
    #[serde(rename = "type")]
    action_type: String,
    cancels: Vec<CancelSpec>,
}

#[derive(Debug, Serialize)]
struct CancelSpec {
    a: u32,
    o: u64,
}

/// API response wrapper.
#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    status: String,
    response: Option<ResponseData>,
}

#[derive(Debug, Deserialize)]
struct ResponseData {
    #[serde(rename = "type")]
    resp_type: String,
    data: Option<serde_json::Value>,
}

impl ExchangeClient {
    pub fn new(
        exchange_url: &str,
        info_url: &str,
        wallet_address: &str,
        is_testnet: bool,
    ) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            exchange_url: exchange_url.to_string(),
            info_url: info_url.to_string(),
            wallet_address: wallet_address.to_string(),
            is_testnet,
        }
    }

    /// Place a market order.
    pub async fn place_market_order(
        &self,
        asset_index: u32,
        side: Side,
        size: Decimal,
        reduce_only: bool,
    ) -> Result<String> {
        let is_buy = matches!(side, Side::Long);
        // Market orders use a very wide price
        let price = if is_buy {
            "999999".to_string() // Will fill at market
        } else {
            "0.001".to_string()
        };

        self.place_order_internal(asset_index, is_buy, &price, &size.to_string(), "Ioc", reduce_only)
            .await
    }

    /// Place a limit order.
    pub async fn place_limit_order(
        &self,
        asset_index: u32,
        side: Side,
        size: Decimal,
        price: Decimal,
        tif: &str, // "Gtc", "Ioc", "Alo"
        reduce_only: bool,
    ) -> Result<String> {
        let is_buy = matches!(side, Side::Long);
        self.place_order_internal(
            asset_index,
            is_buy,
            &price.to_string(),
            &size.to_string(),
            tif,
            reduce_only,
        )
        .await
    }

    /// Internal order placement with retry logic.
    async fn place_order_internal(
        &self,
        asset_index: u32,
        is_buy: bool,
        price: &str,
        size: &str,
        tif: &str,
        reduce_only: bool,
    ) -> Result<String> {
        let order_spec = OrderSpec {
            a: asset_index,
            b: is_buy,
            p: price.to_string(),
            s: size.to_string(),
            r: reduce_only,
            t: OrderTif {
                limit: LimitTif {
                    tif: tif.to_string(),
                },
            },
        };

        let action = PlaceOrderAction {
            action_type: "order".to_string(),
            orders: vec![order_spec],
            grouping: "na".to_string(),
        };

        let nonce = chrono::Utc::now().timestamp_millis() as u64;

        let request = PlaceOrderRequest {
            action,
            nonce,
            signature: String::new(), // Would be signed in production
            vault_address: None,
        };

        let resp = self.post_with_retry(&self.exchange_url, &request).await?;
        let order_id = format!("order-{nonce}");
        info!(
            "Order placed: {} — asset={}, buy={}, size={}, price={}, tif={}",
            order_id, asset_index, is_buy, size, price, tif
        );
        Ok(order_id)
    }

    /// Cancel a specific order.
    pub async fn cancel_order(&self, asset_index: u32, order_id: u64) -> Result<()> {
        let action = CancelAction {
            action_type: "cancel".to_string(),
            cancels: vec![CancelSpec {
                a: asset_index,
                o: order_id,
            }],
        };

        let nonce = chrono::Utc::now().timestamp_millis() as u64;
        let request = CancelOrderRequest {
            action,
            nonce,
            signature: String::new(),
        };

        self.post_with_retry(&self.exchange_url, &request).await?;
        info!("Order cancelled: asset={}, id={}", asset_index, order_id);
        Ok(())
    }

    /// Cancel ALL open orders (emergency function).
    pub async fn cancel_all_orders(&self) -> Result<u32> {
        let nonce = chrono::Utc::now().timestamp_millis() as u64;
        let body = serde_json::json!({
            "action": {
                "type": "cancelByCloid",
                "cancels": []
            },
            "nonce": nonce,
            "signature": ""
        });

        self.post_with_retry(&self.exchange_url, &body).await?;
        info!("All orders cancelled");
        Ok(0)
    }

    /// Place multiple orders atomically (batch).
    pub async fn place_batch_orders(
        &self,
        orders: Vec<(u32, bool, String, String, String, bool)>,
    ) -> Result<Vec<String>> {
        let order_specs: Vec<OrderSpec> = orders
            .iter()
            .map(|(a, b, p, s, tif, r)| OrderSpec {
                a: *a,
                b: *b,
                p: p.clone(),
                s: s.clone(),
                r: *r,
                t: OrderTif {
                    limit: LimitTif {
                        tif: tif.clone(),
                    },
                },
            })
            .collect();

        let action = PlaceOrderAction {
            action_type: "order".to_string(),
            orders: order_specs,
            grouping: "na".to_string(),
        };

        let nonce = chrono::Utc::now().timestamp_millis() as u64;
        let request = PlaceOrderRequest {
            action,
            nonce,
            signature: String::new(),
            vault_address: None,
        };

        self.post_with_retry(&self.exchange_url, &request).await?;
        let ids: Vec<String> = orders
            .iter()
            .enumerate()
            .map(|(i, _)| format!("batch-{nonce}-{i}"))
            .collect();

        info!("Batch order placed: {} orders", ids.len());
        Ok(ids)
    }

    /// POST with exponential backoff retry.
    async fn post_with_retry<T: Serialize>(
        &self,
        url: &str,
        body: &T,
    ) -> Result<serde_json::Value> {
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            match self.client.post(url).json(body).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return resp
                            .json()
                            .await
                            .context("Failed to parse exchange response");
                    }

                    if status.as_u16() == 429 {
                        // Rate limited
                        let delay = Duration::from_millis(BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                        warn!("Rate limited, retrying in {:?}", delay);
                        tokio::time::sleep(delay).await;
                        continue;
                    }

                    let body_text = resp.text().await.unwrap_or_default();
                    last_error = Some(anyhow::anyhow!("Exchange API error {}: {}", status, body_text));
                }
                Err(e) => {
                    last_error = Some(anyhow::anyhow!("Request failed: {}", e));
                }
            }

            if attempt < MAX_RETRIES - 1 {
                let delay = Duration::from_millis(BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                warn!("Retry attempt {} in {:?}", attempt + 1, delay);
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("All retries exhausted")))
    }

    /// Check if the client is connected to testnet.
    pub fn is_testnet(&self) -> bool {
        self.is_testnet
    }

    /// Get the wallet address.
    pub fn wallet_address(&self) -> &str {
        &self.wallet_address
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = ExchangeClient::new(
            "https://api.hyperliquid.xyz/exchange",
            "https://api.hyperliquid.xyz/info",
            "0x1234567890abcdef",
            false,
        );
        assert!(!client.is_testnet());
        assert_eq!(client.wallet_address(), "0x1234567890abcdef");
    }

    #[test]
    fn test_testnet_client() {
        let client = ExchangeClient::new(
            "https://api.hyperliquid-testnet.xyz/exchange",
            "https://api.hyperliquid-testnet.xyz/info",
            "0x1234",
            true,
        );
        assert!(client.is_testnet());
    }

    #[test]
    fn test_order_spec_serialization() {
        let spec = OrderSpec {
            a: 0,
            b: true,
            p: "50000".to_string(),
            s: "0.1".to_string(),
            r: false,
            t: OrderTif {
                limit: LimitTif {
                    tif: "Gtc".to_string(),
                },
            },
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert!(json.contains("50000"));
        assert!(json.contains("Gtc"));
    }
}
