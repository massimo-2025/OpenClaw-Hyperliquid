use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hl_core::{Fill, MarketContext, Side, Signal, StrategyRiskParams, StrategyType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info};

/// Z-score entry threshold.
const ZSCORE_ENTRY: Decimal = dec!(2.0);
/// Z-score exit threshold.
const ZSCORE_EXIT: Decimal = dec!(0.5);
/// Minimum data points for signal generation.
const MIN_DATA_POINTS: usize = 30;
/// Maximum number of active pair trades.
const MAX_PAIR_TRADES: usize = 5;

/// A trading pair definition.
#[derive(Debug, Clone)]
pub struct PairDef {
    pub asset_a: String,
    pub asset_b: String,
    pub name: String,
}

impl PairDef {
    pub fn new(a: &str, b: &str) -> Self {
        Self {
            asset_a: a.to_string(),
            asset_b: b.to_string(),
            name: format!("{}/{}", a, b),
        }
    }
}

/// Spread tracker with Kalman filter-like hedge ratio estimation.
#[derive(Debug, Clone)]
struct SpreadTracker {
    pair: PairDef,
    /// Rolling price history for both legs.
    prices_a: Vec<Decimal>,
    prices_b: Vec<Decimal>,
    /// Current hedge ratio (estimated via OLS regression).
    hedge_ratio: Decimal,
    /// Spread time series.
    spreads: Vec<Decimal>,
    /// Rolling mean of the spread.
    spread_mean: Decimal,
    /// Rolling standard deviation of the spread.
    spread_std: Decimal,
    /// Current z-score.
    z_score: Decimal,
    /// Rolling correlation.
    correlation: Decimal,
    max_history: usize,
}

impl SpreadTracker {
    fn new(pair: PairDef) -> Self {
        Self {
            pair,
            prices_a: Vec::new(),
            prices_b: Vec::new(),
            hedge_ratio: Decimal::ONE,
            spreads: Vec::new(),
            spread_mean: Decimal::ZERO,
            spread_std: Decimal::ZERO,
            z_score: Decimal::ZERO,
            correlation: Decimal::ZERO,
            max_history: 200,
        }
    }

    fn update(&mut self, price_a: Decimal, price_b: Decimal) {
        self.prices_a.push(price_a);
        self.prices_b.push(price_b);

        // Trim history
        if self.prices_a.len() > self.max_history {
            self.prices_a.remove(0);
            self.prices_b.remove(0);
        }

        if self.prices_a.len() < 2 {
            return;
        }

        // Estimate hedge ratio using simple OLS: β = Cov(A,B) / Var(B)
        self.hedge_ratio = self.compute_hedge_ratio();

        // Compute spread: S = price_a - β * price_b
        let spread = price_a - self.hedge_ratio * price_b;
        self.spreads.push(spread);
        if self.spreads.len() > self.max_history {
            self.spreads.remove(0);
        }

        // Compute rolling stats
        self.recompute_stats();
    }

    fn compute_hedge_ratio(&self) -> Decimal {
        let n = self.prices_a.len();
        if n < 2 {
            return Decimal::ONE;
        }

        let n_dec = Decimal::from(n);
        let sum_a: Decimal = self.prices_a.iter().sum();
        let sum_b: Decimal = self.prices_b.iter().sum();
        let sum_ab: Decimal = self
            .prices_a
            .iter()
            .zip(self.prices_b.iter())
            .map(|(a, b)| *a * *b)
            .sum();
        let sum_bb: Decimal = self.prices_b.iter().map(|b| *b * *b).sum();

        let cov = n_dec * sum_ab - sum_a * sum_b;
        let var = n_dec * sum_bb - sum_b * sum_b;

        if var.is_zero() {
            return Decimal::ONE;
        }

        cov / var
    }

    fn recompute_stats(&mut self) {
        let n = self.spreads.len();
        if n == 0 {
            return;
        }

        let n_dec = Decimal::from(n);
        let sum: Decimal = self.spreads.iter().sum();
        self.spread_mean = sum / n_dec;

        if n >= 2 {
            let var: Decimal = self
                .spreads
                .iter()
                .map(|s| (*s - self.spread_mean) * (*s - self.spread_mean))
                .sum::<Decimal>()
                / (n_dec - Decimal::ONE);

            self.spread_std = decimal_sqrt(var);
        }

        // Z-score of current spread
        if !self.spread_std.is_zero() {
            if let Some(&last) = self.spreads.last() {
                self.z_score = (last - self.spread_mean) / self.spread_std;
            }
        }

        // Compute correlation
        self.correlation = self.compute_correlation();
    }

    fn compute_correlation(&self) -> Decimal {
        let n = self.prices_a.len();
        if n < 2 {
            return Decimal::ZERO;
        }

        let n_dec = Decimal::from(n);
        let sum_a: Decimal = self.prices_a.iter().sum();
        let sum_b: Decimal = self.prices_b.iter().sum();
        let mean_a = sum_a / n_dec;
        let mean_b = sum_b / n_dec;

        let cov: Decimal = self
            .prices_a
            .iter()
            .zip(self.prices_b.iter())
            .map(|(a, b)| (*a - mean_a) * (*b - mean_b))
            .sum();

        let var_a: Decimal = self
            .prices_a
            .iter()
            .map(|a| (*a - mean_a) * (*a - mean_a))
            .sum();

        let var_b: Decimal = self
            .prices_b
            .iter()
            .map(|b| (*b - mean_b) * (*b - mean_b))
            .sum();

        let denom = decimal_sqrt(var_a) * decimal_sqrt(var_b);
        if denom.is_zero() {
            return Decimal::ZERO;
        }

        cov / denom
    }

    fn has_enough_data(&self) -> bool {
        self.spreads.len() >= MIN_DATA_POINTS
    }
}

/// Approximate square root for Decimal.
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

/// Active pair trade.
#[derive(Debug, Clone)]
struct PairTrade {
    pair_name: String,
    side_a: Side,
    side_b: Side,
    entry_z: Decimal,
    opened_at: DateTime<Utc>,
}

/// Statistical Arbitrage / Pairs Trading Strategy.
///
/// Identifies correlated asset pairs and trades mean reversion of their
/// spread. Uses OLS regression for hedge ratio estimation and z-score
/// for entry/exit signals. Market neutral by construction.
pub struct StatArbStrategy {
    name: String,
    active_signals: Vec<Signal>,
    pairs: Vec<PairDef>,
    trackers: HashMap<String, SpreadTracker>,
    active_trades: HashMap<String, PairTrade>,
}

impl StatArbStrategy {
    pub fn new() -> Self {
        let pairs = vec![
            PairDef::new("BTC", "ETH"),
            PairDef::new("SOL", "AVAX"),
            PairDef::new("DOGE", "SHIB"),
            PairDef::new("MATIC", "ARB"),
            PairDef::new("LINK", "UNI"),
        ];

        let trackers: HashMap<String, SpreadTracker> = pairs
            .iter()
            .map(|p| (p.name.clone(), SpreadTracker::new(p.clone())))
            .collect();

        Self {
            name: "StatArb".to_string(),
            active_signals: Vec::new(),
            pairs,
            trackers,
            active_trades: HashMap::new(),
        }
    }
}

impl Default for StatArbStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::Strategy for StatArbStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn strategy_type(&self) -> StrategyType {
        StrategyType::StatArb
    }

    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();
        self.active_signals.clear();

        for pair in &self.pairs.clone() {
            let price_a = ctx.prices.get(&pair.asset_a).copied().unwrap_or_default();
            let price_b = ctx.prices.get(&pair.asset_b).copied().unwrap_or_default();

            if price_a.is_zero() || price_b.is_zero() {
                continue;
            }

            // Update tracker
            if let Some(tracker) = self.trackers.get_mut(&pair.name) {
                tracker.update(price_a, price_b);

                if !tracker.has_enough_data() {
                    debug!("StatArb: Not enough data for {} ({} points)", pair.name, tracker.spreads.len());
                    continue;
                }

                let z = tracker.z_score;

                // Check for exit signals on active trades
                if let Some(trade) = self.active_trades.get(&pair.name) {
                    if z.abs() < ZSCORE_EXIT {
                        info!(
                            "StatArb: Exit signal for {} — z-score={:.3}",
                            pair.name, z
                        );
                        // Close both legs
                        let exit_a = Signal::new(
                            &pair.asset_a,
                            trade.side_a.opposite(),
                            dec!(0.85),
                            dec!(0.01),
                            StrategyType::StatArb,
                            &format!("StatArb exit: {} z={:.3}", pair.name, z),
                        );
                        let exit_b = Signal::new(
                            &pair.asset_b,
                            trade.side_b.opposite(),
                            dec!(0.85),
                            dec!(0.01),
                            StrategyType::StatArb,
                            &format!("StatArb exit: {} z={:.3}", pair.name, z),
                        );
                        signals.push(exit_a);
                        signals.push(exit_b);
                        continue;
                    }
                }

                // Check for entry signals
                if self.active_trades.contains_key(&pair.name)
                    || self.active_trades.len() >= MAX_PAIR_TRADES
                {
                    continue;
                }

                // Minimum correlation requirement
                if tracker.correlation.abs() < dec!(0.5) {
                    debug!("StatArb: Correlation too low for {} ({:.3})", pair.name, tracker.correlation);
                    continue;
                }

                if z.abs() > ZSCORE_ENTRY {
                    // Spread is extended — trade mean reversion
                    // If z > 2: spread is too wide (A expensive relative to B)
                    //   → Short A, Long B
                    // If z < -2: spread is too narrow (A cheap relative to B)
                    //   → Long A, Short B
                    let (side_a, side_b) = if z > Decimal::ZERO {
                        (Side::Short, Side::Long) // Spread will narrow
                    } else {
                        (Side::Long, Side::Short) // Spread will widen back
                    };

                    let confidence = dec!(0.6) + (z.abs() - ZSCORE_ENTRY) * dec!(0.1);
                    let edge = tracker.spread_std / tracker.spread_mean.abs().max(Decimal::ONE);

                    // Size inversely proportional to spread volatility
                    let size_factor = if !tracker.spread_std.is_zero() {
                        (Decimal::ONE / tracker.spread_std).min(dec!(10))
                    } else {
                        Decimal::ONE
                    };

                    let signal_a = Signal::new(
                        &pair.asset_a,
                        side_a,
                        confidence.min(dec!(0.85)),
                        edge,
                        StrategyType::StatArb,
                        &format!(
                            "StatArb entry: {} z={:.3}, corr={:.3}, hedge_ratio={:.4}",
                            pair.name, z, tracker.correlation, tracker.hedge_ratio
                        ),
                    )
                    .with_leverage(dec!(2));

                    let signal_b = Signal::new(
                        &pair.asset_b,
                        side_b,
                        confidence.min(dec!(0.85)),
                        edge,
                        StrategyType::StatArb,
                        &format!(
                            "StatArb entry: {} z={:.3}, hedge_ratio={:.4}",
                            pair.name, z, tracker.hedge_ratio
                        ),
                    )
                    .with_leverage(dec!(2));

                    info!(
                        "StatArb signal: {} — z={:.3}, corr={:.3}, sides={}/{}",
                        pair.name, z, tracker.correlation, side_a, side_b
                    );

                    self.active_signals.push(signal_a.clone());
                    self.active_signals.push(signal_b.clone());
                    signals.push(signal_a);
                    signals.push(signal_b);
                }
            }
        }

        Ok(signals)
    }

    async fn on_fill(&mut self, fill: &Fill) -> Result<()> {
        // Find which pair this fill belongs to
        for pair in &self.pairs {
            if fill.asset == pair.asset_a || fill.asset == pair.asset_b {
                if !self.active_trades.contains_key(&pair.name) {
                    let (side_a, side_b) = if fill.asset == pair.asset_a {
                        (fill.side, fill.side.opposite())
                    } else {
                        (fill.side.opposite(), fill.side)
                    };

                    self.active_trades.insert(
                        pair.name.clone(),
                        PairTrade {
                            pair_name: pair.name.clone(),
                            side_a,
                            side_b,
                            entry_z: self
                                .trackers
                                .get(&pair.name)
                                .map(|t| t.z_score)
                                .unwrap_or_default(),
                            opened_at: Utc::now(),
                        },
                    );
                } else {
                    // Closing the trade
                    self.active_trades.remove(&pair.name);
                }
                break;
            }
        }
        Ok(())
    }

    fn active_signals(&self) -> &[Signal] {
        &self.active_signals
    }

    fn risk_params(&self) -> StrategyRiskParams {
        StrategyRiskParams {
            max_position_pct: dec!(8),
            max_leverage: dec!(2),
            max_drawdown_pct: dec!(5),
            max_correlated_exposure: dec!(25),
            cooldown_secs: 300,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::Portfolio;

    #[test]
    fn test_pair_def() {
        let pair = PairDef::new("BTC", "ETH");
        assert_eq!(pair.name, "BTC/ETH");
    }

    #[test]
    fn test_spread_tracker_hedge_ratio() {
        let pair = PairDef::new("BTC", "ETH");
        let mut tracker = SpreadTracker::new(pair);

        // Add correlated prices
        for i in 0..50 {
            let btc = dec!(50000) + Decimal::new(i * 100, 0);
            let eth = dec!(3000) + Decimal::new(i * 6, 0);
            tracker.update(btc, eth);
        }

        // Hedge ratio should be approximately BTC/ETH price ratio
        assert!(tracker.hedge_ratio > Decimal::ZERO);
        assert!(tracker.has_enough_data());
    }

    #[test]
    fn test_spread_tracker_z_score() {
        let pair = PairDef::new("A", "B");
        let mut tracker = SpreadTracker::new(pair);

        // Add data with stable relationship
        for i in 0..40 {
            let a = dec!(100) + Decimal::new(i, 1);
            let b = dec!(50) + Decimal::new(i, 2);
            tracker.update(a, b);
        }

        // Z-score should be near zero for the last point in a stable series
        assert!(tracker.z_score.abs() < dec!(3));
    }

    #[test]
    fn test_correlation_computation() {
        let pair = PairDef::new("A", "B");
        let mut tracker = SpreadTracker::new(pair);

        // Perfectly correlated
        for i in 0..50 {
            tracker.update(Decimal::from(i), Decimal::from(i * 2));
        }

        assert!(tracker.correlation > dec!(0.99));
    }

    #[test]
    fn test_decimal_sqrt() {
        let result = decimal_sqrt(dec!(16));
        assert!((result - dec!(4)).abs() < dec!(0.0001));
    }

    #[tokio::test]
    async fn test_stat_arb_initialization() {
        let strategy = StatArbStrategy::new();
        assert_eq!(strategy.pairs.len(), 5);
        assert_eq!(strategy.trackers.len(), 5);
    }

    #[tokio::test]
    async fn test_stat_arb_insufficient_data() {
        let mut strategy = StatArbStrategy::new();
        let mut prices = HashMap::new();
        prices.insert("BTC".to_string(), dec!(50000));
        prices.insert("ETH".to_string(), dec!(3000));

        let ctx = MarketContext {
            assets: Vec::new(),
            prices,
            funding_rates: HashMap::new(),
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(10000)),
            timestamp: Utc::now(),
        };

        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert!(signals.is_empty()); // Not enough data
    }

    #[test]
    fn test_risk_params() {
        let strategy = StatArbStrategy::new();
        let params = strategy.risk_params();
        assert_eq!(params.max_leverage, dec!(2));
        assert_eq!(params.max_position_pct, dec!(8));
    }
}
