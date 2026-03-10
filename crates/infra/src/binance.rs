use anyhow::{Context, Result};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, warn};

/// Binance futures API client for cross-exchange arbitrage data.
/// Fetches funding rates and prices from Binance for comparison with Hyperliquid.
pub struct BinanceClient {
    client: Client,
    base_url: String,
}

#[derive(Debug, Deserialize)]
struct BinanceFundingRate {
    symbol: String,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
    #[serde(rename = "fundingTime")]
    funding_time: u64,
    #[serde(rename = "markPrice")]
    mark_price: String,
}

#[derive(Debug, Deserialize)]
struct BinancePrice {
    symbol: String,
    price: String,
}

#[derive(Debug, Clone)]
pub struct CrossExchangeFunding {
    pub asset: String,
    pub hl_rate: Decimal,
    pub binance_rate: Decimal,
    pub differential: Decimal,
    pub hl_annualized: Decimal,
    pub binance_annualized: Decimal,
}

impl BinanceClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            base_url: "https://fapi.binance.com".to_string(),
        }
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    /// Fetch current funding rates for all perpetual pairs.
    pub async fn get_funding_rates(&self) -> Result<HashMap<String, Decimal>> {
        let url = format!("{}/fapi/v1/premiumIndex", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch Binance funding rates")?;

        let rates: Vec<BinanceFundingRate> = resp
            .json()
            .await
            .context("Failed to parse Binance funding rates")?;

        let mut result = HashMap::new();
        for rate in rates {
            // Convert Binance symbol (BTCUSDT) to asset name (BTC)
            let asset = rate
                .symbol
                .strip_suffix("USDT")
                .unwrap_or(&rate.symbol)
                .to_string();

            if let Ok(r) = rate.funding_rate.parse::<Decimal>() {
                result.insert(asset, r);
            }
        }

        debug!("Fetched {} Binance funding rates", result.len());
        Ok(result)
    }

    /// Fetch current prices for all perpetual pairs.
    pub async fn get_prices(&self) -> Result<HashMap<String, Decimal>> {
        let url = format!("{}/fapi/v1/ticker/price", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch Binance prices")?;

        let prices: Vec<BinancePrice> = resp
            .json()
            .await
            .context("Failed to parse Binance prices")?;

        let mut result = HashMap::new();
        for price in prices {
            let asset = price
                .symbol
                .strip_suffix("USDT")
                .unwrap_or(&price.symbol)
                .to_string();

            if let Ok(p) = price.price.parse::<Decimal>() {
                result.insert(asset, p);
            }
        }

        Ok(result)
    }

    /// Compare funding rates between Hyperliquid and Binance.
    pub fn compare_funding(
        hl_rates: &HashMap<String, Decimal>,
        binance_rates: &HashMap<String, Decimal>,
    ) -> Vec<CrossExchangeFunding> {
        let annualize = |rate: Decimal| rate * Decimal::new(8760, 0); // 24 * 365

        let mut comparisons = Vec::new();

        for (asset, &hl_rate) in hl_rates {
            if let Some(&binance_rate) = binance_rates.get(asset) {
                let differential = hl_rate - binance_rate;
                comparisons.push(CrossExchangeFunding {
                    asset: asset.clone(),
                    hl_rate,
                    binance_rate,
                    differential,
                    hl_annualized: annualize(hl_rate),
                    binance_annualized: annualize(binance_rate),
                });
            }
        }

        // Sort by absolute differential (descending)
        comparisons.sort_by(|a, b| {
            b.differential
                .abs()
                .partial_cmp(&a.differential.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        comparisons
    }
}

impl Default for BinanceClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_compare_funding() {
        let mut hl_rates = HashMap::new();
        hl_rates.insert("BTC".to_string(), dec!(0.0003));
        hl_rates.insert("ETH".to_string(), dec!(0.0005));
        hl_rates.insert("SOL".to_string(), dec!(0.001));

        let mut binance_rates = HashMap::new();
        binance_rates.insert("BTC".to_string(), dec!(0.0001));
        binance_rates.insert("ETH".to_string(), dec!(0.0004));
        binance_rates.insert("DOGE".to_string(), dec!(0.002)); // Not on HL

        let comparisons = BinanceClient::compare_funding(&hl_rates, &binance_rates);

        // Should only have 2 matches (BTC, ETH). SOL not on Binance, DOGE not on HL.
        assert_eq!(comparisons.len(), 2);

        // BTC differential = 0.0003 - 0.0001 = 0.0002
        let btc = comparisons.iter().find(|c| c.asset == "BTC").unwrap();
        assert_eq!(btc.differential, dec!(0.0002));
    }

    #[test]
    fn test_compare_funding_sorted() {
        let mut hl_rates = HashMap::new();
        hl_rates.insert("BTC".to_string(), dec!(0.0001));
        hl_rates.insert("SOL".to_string(), dec!(0.005));

        let mut binance_rates = HashMap::new();
        binance_rates.insert("BTC".to_string(), dec!(0.0001));
        binance_rates.insert("SOL".to_string(), dec!(0.001));

        let comparisons = BinanceClient::compare_funding(&hl_rates, &binance_rates);

        // SOL should be first (larger differential)
        assert_eq!(comparisons[0].asset, "SOL");
    }

    #[test]
    fn test_client_creation() {
        let client = BinanceClient::new();
        assert_eq!(client.base_url, "https://fapi.binance.com");
    }

    #[test]
    fn test_custom_base_url() {
        let client = BinanceClient::new().with_base_url("https://testnet.binancefuture.com");
        assert_eq!(client.base_url, "https://testnet.binancefuture.com");
    }
}
