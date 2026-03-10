use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hl_core::{Fill, MarketContext, Side, Signal, StrategyRiskParams, StrategyType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info};

/// Entry threshold: basis > 0.5% = signal.
const BASIS_ENTRY_THRESHOLD: Decimal = dec!(0.005);
/// Exit threshold: basis < 0.1% = close.
const BASIS_EXIT_THRESHOLD: Decimal = dec!(0.001);
/// Maximum number of tracked basis positions.
const MAX_BASIS_POSITIONS: usize = 5;
/// Minimum time series length for mean reversion signals.
const MIN_HISTORY_FOR_MEAN_REVERSION: usize = 24;

/// Tracks basis (spot - perp mark price) over time.
#[derive(Debug, Clone)]
struct BasisHistory {
    asset: String,
    /// (timestamp, basis_pct) time series.
    history: Vec<(DateTime<Utc>, Decimal)>,
    mean: Decimal,
    std_dev: Decimal,
}

impl BasisHistory {
    fn new(asset: &str) -> Self {
        Self {
            asset: asset.to_string(),
            history: Vec::new(),
            mean: Decimal::ZERO,
            std_dev: Decimal::ZERO,
        }
    }

    fn push(&mut self, basis_pct: Decimal) {
        self.history.push((Utc::now(), basis_pct));
        if self.history.len() > 720 {
            self.history.remove(0);
        }
        self.recompute_stats();
    }

    fn recompute_stats(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let n = Decimal::from(self.history.len());
        let sum: Decimal = self.history.iter().map(|(_, b)| *b).sum();
        self.mean = sum / n;

        if self.history.len() >= 2 {
            let var: Decimal = self
                .history
                .iter()
                .map(|(_, b)| (*b - self.mean) * (*b - self.mean))
                .sum::<Decimal>()
                / (n - Decimal::ONE);
            // Approximate sqrt via Newton's method for Decimal
            self.std_dev = decimal_sqrt(var);
        }
    }

    fn z_score(&self, current: Decimal) -> Decimal {
        if self.std_dev.is_zero() {
            return Decimal::ZERO;
        }
        (current - self.mean) / self.std_dev
    }
}

/// Approximate square root for Decimal using Newton's method.
fn decimal_sqrt(val: Decimal) -> Decimal {
    if val <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let mut x = val;
    for _ in 0..20 {
        let next = (x + val / x) / dec!(2);
        if (next - x).abs() < dec!(0.0000001) {
            return next;
        }
        x = next;
    }
    x
}

/// Tracked basis position.
#[derive(Debug, Clone)]
struct BasisPosition {
    asset: String,
    entry_basis: Decimal,
    opened_at: DateTime<Utc>,
}

/// Spot-Perp Basis Trade Strategy.
///
/// Monitors the basis (difference between spot and perp mark price).
/// When basis widens beyond threshold, enters a convergence trade:
/// - Buy spot + Short perp when basis is positive (premium)
/// - Profit from basis convergence + collect funding while waiting
pub struct BasisTradeStrategy {
    name: String,
    active_signals: Vec<Signal>,
    tracked_positions: HashMap<String, BasisPosition>,
    basis_histories: HashMap<String, BasisHistory>,
}

impl BasisTradeStrategy {
    pub fn new() -> Self {
        Self {
            name: "BasisTrade".to_string(),
            active_signals: Vec::new(),
            tracked_positions: HashMap::new(),
            basis_histories: HashMap::new(),
        }
    }

    /// Compute basis as a percentage: (mark_price - spot_price) / spot_price.
    /// Positive = perp trades at premium, Negative = perp trades at discount.
    fn compute_basis(spot_price: Decimal, mark_price: Decimal) -> Decimal {
        if spot_price.is_zero() {
            return Decimal::ZERO;
        }
        (mark_price - spot_price) / spot_price
    }

    /// Compute confidence based on basis magnitude and history.
    fn compute_confidence(&self, asset: &str, basis_pct: Decimal) -> Decimal {
        let base = if basis_pct.abs() > dec!(0.02) {
            dec!(0.85)
        } else if basis_pct.abs() > dec!(0.01) {
            dec!(0.75)
        } else {
            dec!(0.65)
        };

        // Bonus if mean reversion is strong (z-score > 2)
        let z_bonus = if let Some(history) = self.basis_histories.get(asset) {
            let z = history.z_score(basis_pct);
            if z.abs() > dec!(2.5) {
                dec!(0.05)
            } else {
                Decimal::ZERO
            }
        } else {
            Decimal::ZERO
        };

        (base + z_bonus).min(dec!(0.95))
    }
}

impl Default for BasisTradeStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::Strategy for BasisTradeStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn strategy_type(&self) -> StrategyType {
        StrategyType::BasisTrade
    }

    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();
        self.active_signals.clear();

        for asset_info in &ctx.assets {
            let asset = &asset_info.name;
            let mark_price = asset_info.mark_price;

            // Use mid price from orderbook as proxy for spot price
            // (Hyperliquid has spot orderbook for some assets)
            let spot_price = ctx
                .prices
                .get(asset)
                .copied()
                .unwrap_or(mark_price);

            if spot_price.is_zero() || mark_price.is_zero() {
                continue;
            }

            let basis = Self::compute_basis(spot_price, mark_price);

            // Track basis history
            let history = self
                .basis_histories
                .entry(asset.clone())
                .or_insert_with(|| BasisHistory::new(asset));
            history.push(basis);

            // Check exit conditions for existing positions
            if let Some(pos) = self.tracked_positions.get(asset) {
                if basis.abs() < BASIS_EXIT_THRESHOLD {
                    info!(
                        "BasisTrade: Exit signal for {} — basis {:.4}% below threshold",
                        asset,
                        basis * dec!(100)
                    );
                    let signal = Signal::new(
                        asset,
                        Side::Long, // Close the short perp leg
                        dec!(0.9),
                        dec!(0.01),
                        StrategyType::BasisTrade,
                        &format!(
                            "Exit basis trade: basis converged to {:.4}%, entry was {:.4}%",
                            basis * dec!(100),
                            pos.entry_basis * dec!(100)
                        ),
                    );
                    signals.push(signal);
                    continue;
                }
            }

            // Check entry conditions
            if basis.abs() > BASIS_ENTRY_THRESHOLD
                && !self.tracked_positions.contains_key(asset)
                && self.tracked_positions.len() < MAX_BASIS_POSITIONS
            {
                // Positive basis (premium): short perp, buy spot
                // Negative basis (discount): long perp, sell spot
                let side = if basis > Decimal::ZERO {
                    Side::Short // Short the premium
                } else {
                    Side::Long // Long the discount
                };

                let confidence = self.compute_confidence(asset, basis);
                let edge = basis.abs();

                let signal = Signal::new(
                    asset,
                    side,
                    confidence,
                    edge,
                    StrategyType::BasisTrade,
                    &format!(
                        "Basis trade: basis={:.4}%, spot={}, mark={}",
                        basis * dec!(100),
                        spot_price,
                        mark_price
                    ),
                )
                .with_leverage(dec!(1));

                info!(
                    "BasisTrade signal: {} {} — basis {:.4}%",
                    side,
                    asset,
                    basis * dec!(100)
                );

                self.active_signals.push(signal.clone());
                signals.push(signal);
            }
        }

        Ok(signals)
    }

    async fn on_fill(&mut self, fill: &Fill) -> Result<()> {
        if !self.tracked_positions.contains_key(&fill.asset) {
            self.tracked_positions.insert(
                fill.asset.clone(),
                BasisPosition {
                    asset: fill.asset.clone(),
                    entry_basis: Decimal::ZERO,
                    opened_at: Utc::now(),
                },
            );
        } else {
            self.tracked_positions.remove(&fill.asset);
        }
        Ok(())
    }

    fn active_signals(&self) -> &[Signal] {
        &self.active_signals
    }

    fn risk_params(&self) -> StrategyRiskParams {
        StrategyRiskParams {
            max_position_pct: dec!(10),
            max_leverage: dec!(1),
            max_drawdown_pct: dec!(3),
            max_correlated_exposure: dec!(20),
            cooldown_secs: 600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::{AssetInfo, Portfolio};

    fn make_context_with_basis(
        asset: &str,
        spot: Decimal,
        mark: Decimal,
    ) -> MarketContext {
        let mut prices = HashMap::new();
        prices.insert(asset.to_string(), spot);

        let assets = vec![AssetInfo {
            name: asset.to_string(),
            sz_decimals: 4,
            max_leverage: 50,
            mark_price: mark,
            funding_rate: dec!(0.0001),
            open_interest: dec!(1000000),
            volume_24h: dec!(50000000),
        }];

        MarketContext {
            assets,
            prices,
            funding_rates: HashMap::new(),
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(10000)),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn test_compute_basis() {
        let basis = BasisTradeStrategy::compute_basis(dec!(50000), dec!(50250));
        assert_eq!(basis, dec!(0.005)); // 0.5% premium
    }

    #[test]
    fn test_compute_basis_discount() {
        let basis = BasisTradeStrategy::compute_basis(dec!(50000), dec!(49750));
        assert_eq!(basis, dec!(-0.005)); // 0.5% discount
    }

    #[test]
    fn test_compute_basis_zero_spot() {
        let basis = BasisTradeStrategy::compute_basis(Decimal::ZERO, dec!(50000));
        assert_eq!(basis, Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_basis_entry_signal_premium() {
        let mut strategy = BasisTradeStrategy::new();
        let ctx = make_context_with_basis("BTC", dec!(50000), dec!(50500)); // 1% premium

        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].side, Side::Short); // Short the premium
    }

    #[tokio::test]
    async fn test_basis_entry_signal_discount() {
        let mut strategy = BasisTradeStrategy::new();
        let ctx = make_context_with_basis("ETH", dec!(3000), dec!(2970)); // 1% discount

        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].side, Side::Long); // Long the discount
    }

    #[tokio::test]
    async fn test_basis_no_signal_small_basis() {
        let mut strategy = BasisTradeStrategy::new();
        let ctx = make_context_with_basis("BTC", dec!(50000), dec!(50010)); // tiny basis

        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn test_decimal_sqrt() {
        let result = decimal_sqrt(dec!(4));
        assert!((result - dec!(2)).abs() < dec!(0.0001));

        let result = decimal_sqrt(dec!(9));
        assert!((result - dec!(3)).abs() < dec!(0.0001));
    }

    #[test]
    fn test_basis_history() {
        let mut history = BasisHistory::new("BTC");
        for i in 0..10 {
            history.push(Decimal::new(i, 3));
        }
        assert_eq!(history.history.len(), 10);
        assert!(history.mean > Decimal::ZERO);
    }
}
