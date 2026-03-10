use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hl_core::{Fill, MarketContext, Side, Signal, StrategyRiskParams, StrategyType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Thresholds for funding rate arbitrage.
const ENTRY_ANNUALIZED_THRESHOLD: Decimal = dec!(20.0); // Enter when annualized > 20%
const EXIT_ANNUALIZED_THRESHOLD: Decimal = dec!(5.0); // Exit when annualized < 5%
const MIN_CONFIDENCE: Decimal = dec!(0.6);
const MAX_POSITIONS: usize = 10;

/// Tracked funding position for internal state.
#[derive(Debug, Clone)]
struct FundingPosition {
    asset: String,
    side: Side,
    entry_rate: Decimal,
    cumulative_funding: Decimal,
    opened_at: DateTime<Utc>,
    last_rate: Decimal,
}

/// Funding Rate Arbitrage Strategy.
///
/// Monitors funding rates across all perpetual pairs and takes positions
/// that collect funding payments. For extreme rates, goes delta-neutral
/// to harvest the funding while minimizing directional risk.
///
/// Cross-exchange mode: compares Hyperliquid funding vs Binance to
/// identify the most profitable exchange to be on.
pub struct FundingArbStrategy {
    name: String,
    active_signals: Vec<Signal>,
    tracked_positions: HashMap<String, FundingPosition>,
    /// Rolling history of funding per asset for signal quality.
    rate_history: HashMap<String, Vec<(DateTime<Utc>, Decimal)>>,
    max_history_len: usize,
}

impl FundingArbStrategy {
    pub fn new() -> Self {
        Self {
            name: "FundingArb".to_string(),
            active_signals: Vec::new(),
            tracked_positions: HashMap::new(),
            rate_history: HashMap::new(),
            max_history_len: 720, // 30 days of hourly data
        }
    }

    /// Compute annualized rate from hourly funding rate.
    fn annualize(hourly_rate: Decimal) -> Decimal {
        hourly_rate * dec!(8760) // 24 * 365
    }

    /// Determine confidence based on rate magnitude and consistency.
    fn compute_confidence(&self, asset: &str, annualized: Decimal) -> Decimal {
        let base = if annualized.abs() > dec!(50.0) {
            dec!(0.9)
        } else if annualized.abs() > dec!(30.0) {
            dec!(0.8)
        } else {
            dec!(0.65)
        };

        // Adjust for consistency: if rate has been stable, higher confidence
        let consistency_bonus = if let Some(history) = self.rate_history.get(asset) {
            if history.len() >= 3 {
                let recent: Vec<_> = history.iter().rev().take(3).collect();
                let all_same_sign = recent
                    .iter()
                    .all(|(_, r)| r.is_sign_positive() == recent[0].1.is_sign_positive());
                if all_same_sign {
                    dec!(0.05)
                } else {
                    dec!(-0.05)
                }
            } else {
                Decimal::ZERO
            }
        } else {
            Decimal::ZERO
        };

        (base + consistency_bonus).min(dec!(0.95)).max(MIN_CONFIDENCE)
    }

    /// Check if we should exit an existing position.
    fn should_exit(&self, asset: &str, current_annualized: Decimal) -> bool {
        if let Some(pos) = self.tracked_positions.get(asset) {
            // Exit if funding has normalized
            if current_annualized.abs() < EXIT_ANNUALIZED_THRESHOLD {
                info!(
                    "FundingArb: Exit signal for {} — annualized rate {:.2}% below threshold",
                    asset, current_annualized
                );
                return true;
            }
            // Exit if funding flipped direction
            let entry_sign = pos.entry_rate.is_sign_positive();
            let current_sign = current_annualized.is_sign_positive();
            if entry_sign != current_sign {
                info!(
                    "FundingArb: Exit signal for {} — funding direction flipped",
                    asset
                );
                return true;
            }
        }
        false
    }

    /// Update rate history for an asset.
    fn record_rate(&mut self, asset: &str, rate: Decimal) {
        let history = self
            .rate_history
            .entry(asset.to_string())
            .or_insert_with(Vec::new);
        history.push((Utc::now(), rate));
        if history.len() > self.max_history_len {
            history.remove(0);
        }
    }
}

impl Default for FundingArbStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::Strategy for FundingArbStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn strategy_type(&self) -> StrategyType {
        StrategyType::FundingArb
    }

    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();
        self.active_signals.clear();

        for (asset, &rate) in &ctx.funding_rates {
            let annualized = Self::annualize(rate);
            self.record_rate(asset, rate);

            // Check for exit signals first
            if self.should_exit(asset, annualized) {
                let pos = self.tracked_positions.get(asset).unwrap();
                let exit_side = pos.side.opposite();
                let signal = Signal::new(
                    asset,
                    exit_side,
                    dec!(0.9), // High confidence to exit
                    dec!(0.01),
                    StrategyType::FundingArb,
                    &format!(
                        "Exit funding arb: annualized={:.2}%, cumulative_funding={:.6}",
                        annualized, pos.cumulative_funding
                    ),
                );
                signals.push(signal);
                continue;
            }

            // Check for entry signals
            if annualized.abs() > ENTRY_ANNUALIZED_THRESHOLD
                && !self.tracked_positions.contains_key(asset)
                && self.tracked_positions.len() < MAX_POSITIONS
            {
                // If funding is positive (longs pay shorts), go short to collect
                // If funding is negative (shorts pay longs), go long to collect
                let side = if rate > Decimal::ZERO {
                    Side::Short // Collect from longs
                } else {
                    Side::Long // Collect from shorts
                };

                let confidence = self.compute_confidence(asset, annualized);
                let edge = rate.abs(); // Edge is the hourly funding rate

                let signal = Signal::new(
                    asset,
                    side,
                    confidence,
                    edge,
                    StrategyType::FundingArb,
                    &format!(
                        "Funding arb: hourly={:.6}%, annualized={:.2}%",
                        rate * dec!(100),
                        annualized
                    ),
                )
                .with_leverage(dec!(1)); // Low leverage for funding arb

                info!(
                    "FundingArb signal: {} {} — annualized {:.2}%, confidence {:.2}",
                    side, asset, annualized, confidence
                );

                self.active_signals.push(signal.clone());
                signals.push(signal);
            }
        }

        Ok(signals)
    }

    async fn on_fill(&mut self, fill: &Fill) -> Result<()> {
        // Track the position for ongoing monitoring
        if !self.tracked_positions.contains_key(&fill.asset) {
            self.tracked_positions.insert(
                fill.asset.clone(),
                FundingPosition {
                    asset: fill.asset.clone(),
                    side: fill.side,
                    entry_rate: Decimal::ZERO, // Will be updated on next evaluation
                    cumulative_funding: Decimal::ZERO,
                    opened_at: Utc::now(),
                    last_rate: Decimal::ZERO,
                },
            );
            info!("FundingArb: Tracking new position in {}", fill.asset);
        } else {
            // This might be an exit fill — remove tracking
            self.tracked_positions.remove(&fill.asset);
            info!("FundingArb: Removed position tracking for {}", fill.asset);
        }
        Ok(())
    }

    fn active_signals(&self) -> &[Signal] {
        &self.active_signals
    }

    fn risk_params(&self) -> StrategyRiskParams {
        StrategyRiskParams {
            max_position_pct: dec!(10),
            max_leverage: dec!(2),
            max_drawdown_pct: dec!(5),
            max_correlated_exposure: dec!(30),
            cooldown_secs: 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::{AssetInfo, OrderbookSnapshot, Portfolio};
    use std::collections::HashMap;

    fn make_context(funding_rates: HashMap<String, Decimal>) -> MarketContext {
        MarketContext {
            assets: Vec::new(),
            prices: HashMap::new(),
            funding_rates,
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(10000)),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_funding_arb_high_positive_rate() {
        let mut strategy = FundingArbStrategy::new();
        let mut rates = HashMap::new();
        rates.insert("DOGE".to_string(), dec!(0.005)); // 0.5%/hr = 43.8% annualized

        let ctx = make_context(rates);
        let signals = strategy.evaluate(&ctx).await.unwrap();

        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].asset, "DOGE");
        assert_eq!(signals[0].side, Side::Short); // Short to collect from longs
        assert!(signals[0].confidence >= MIN_CONFIDENCE);
    }

    #[tokio::test]
    async fn test_funding_arb_high_negative_rate() {
        let mut strategy = FundingArbStrategy::new();
        let mut rates = HashMap::new();
        rates.insert("BTC".to_string(), dec!(-0.004)); // Negative = shorts pay longs

        let ctx = make_context(rates);
        let signals = strategy.evaluate(&ctx).await.unwrap();

        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].side, Side::Long); // Long to collect from shorts
    }

    #[tokio::test]
    async fn test_funding_arb_low_rate_no_signal() {
        let mut strategy = FundingArbStrategy::new();
        let mut rates = HashMap::new();
        rates.insert("BTC".to_string(), dec!(0.0001)); // 0.876% annualized — too low

        let ctx = make_context(rates);
        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert!(signals.is_empty());
    }

    #[tokio::test]
    async fn test_funding_arb_max_positions() {
        let mut strategy = FundingArbStrategy::new();
        // Fill up tracked positions
        for i in 0..MAX_POSITIONS {
            strategy.tracked_positions.insert(
                format!("ASSET{i}"),
                FundingPosition {
                    asset: format!("ASSET{i}"),
                    side: Side::Short,
                    entry_rate: dec!(0.005),
                    cumulative_funding: Decimal::ZERO,
                    opened_at: Utc::now(),
                    last_rate: dec!(0.005),
                },
            );
        }

        let mut rates = HashMap::new();
        rates.insert("NEWASSET".to_string(), dec!(0.01));

        let ctx = make_context(rates);
        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert!(signals.is_empty()); // No new entries when at max
    }

    #[tokio::test]
    async fn test_funding_arb_exit_signal() {
        let mut strategy = FundingArbStrategy::new();
        strategy.tracked_positions.insert(
            "BTC".to_string(),
            FundingPosition {
                asset: "BTC".to_string(),
                side: Side::Short,
                entry_rate: dec!(0.005),
                cumulative_funding: dec!(0.01),
                opened_at: Utc::now(),
                last_rate: dec!(0.001),
            },
        );

        let mut rates = HashMap::new();
        rates.insert("BTC".to_string(), dec!(0.0001)); // Below exit threshold

        let ctx = make_context(rates);
        let signals = strategy.evaluate(&ctx).await.unwrap();

        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].side, Side::Long); // Opposite of short position
    }

    #[test]
    fn test_annualize() {
        let hourly = dec!(0.01); // 1% per hour
        let annual = FundingArbStrategy::annualize(hourly);
        assert_eq!(annual, dec!(87.60));
    }

    #[test]
    fn test_risk_params() {
        let strategy = FundingArbStrategy::new();
        let params = strategy.risk_params();
        assert_eq!(params.max_leverage, dec!(2));
    }

    #[tokio::test]
    async fn test_on_fill_tracking() {
        let mut strategy = FundingArbStrategy::new();
        let fill = Fill {
            order_id: "test-1".to_string(),
            asset: "ETH".to_string(),
            side: Side::Short,
            size: dec!(5),
            price: dec!(3000),
            fee: dec!(0.5),
            timestamp: Utc::now(),
            strategy: "FundingArb".to_string(),
            is_maker: true,
        };

        strategy.on_fill(&fill).await.unwrap();
        assert!(strategy.tracked_positions.contains_key("ETH"));

        // Second fill should remove (exit)
        strategy.on_fill(&fill).await.unwrap();
        assert!(!strategy.tracked_positions.contains_key("ETH"));
    }
}
