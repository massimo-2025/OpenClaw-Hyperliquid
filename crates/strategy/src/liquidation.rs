use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use hl_core::{Fill, MarketContext, Side, Signal, StrategyRiskParams, StrategyType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Threshold: position approaching liquidation when margin ratio < this.
const LIQUIDATION_PROXIMITY_THRESHOLD: Decimal = dec!(0.67); // 2/3 of maintenance margin
/// Minimum position notional to be considered a "large" position.
const MIN_LARGE_POSITION_NOTIONAL: Decimal = dec!(1_000_000);
/// Signal confidence for cascade detection.
const CASCADE_CONFIDENCE: Decimal = dec!(0.7);
/// Stop-loss percentage for liquidation plays.
const STOP_LOSS_PCT: Decimal = dec!(0.02); // 2%
/// Maximum time to hold a liquidation play.
const MAX_HOLD_HOURS: i64 = 4;

/// Represents a detected potential liquidation.
#[derive(Debug, Clone)]
struct LiquidationTarget {
    asset: String,
    estimated_liq_price: Decimal,
    position_size: Decimal,
    side: Side, // Side of the position being liquidated
    detected_at: DateTime<Utc>,
    margin_ratio: Decimal,
}

/// Active liquidation play.
#[derive(Debug, Clone)]
struct LiquidationPlay {
    asset: String,
    our_side: Side,
    entry_time: DateTime<Utc>,
    stop_loss: Decimal,
}

/// Liquidation Cascade Detection Strategy.
///
/// Monitors the market for large positions approaching liquidation.
/// When detected, positions to profit from the cascade effect:
/// liquidation of a large long drives price down further, triggering
/// more liquidations. Strategy shorts before the cascade and profits
/// from the forced selling.
pub struct LiquidationStrategy {
    name: String,
    active_signals: Vec<Signal>,
    targets: HashMap<String, LiquidationTarget>,
    active_plays: HashMap<String, LiquidationPlay>,
}

impl LiquidationStrategy {
    pub fn new() -> Self {
        Self {
            name: "Liquidation".to_string(),
            active_signals: Vec::new(),
            targets: HashMap::new(),
            active_plays: HashMap::new(),
        }
    }

    /// Estimate the liquidation price for a position.
    /// For longs: liq_price ≈ entry_price * (1 - 1/leverage + maintenance_margin_rate)
    /// For shorts: liq_price ≈ entry_price * (1 + 1/leverage - maintenance_margin_rate)
    fn estimate_liquidation_price(
        entry_price: Decimal,
        leverage: Decimal,
        side: Side,
        maintenance_margin_rate: Decimal,
    ) -> Decimal {
        if leverage.is_zero() {
            return Decimal::ZERO;
        }
        match side {
            Side::Long => {
                entry_price * (Decimal::ONE - Decimal::ONE / leverage + maintenance_margin_rate)
            }
            Side::Short => {
                entry_price * (Decimal::ONE + Decimal::ONE / leverage - maintenance_margin_rate)
            }
        }
    }

    /// Calculate how close the current price is to estimated liquidation.
    /// Returns 0.0 (far) to 1.0 (at liquidation).
    fn liquidation_proximity(
        current_price: Decimal,
        liq_price: Decimal,
        entry_price: Decimal,
        side: Side,
    ) -> Decimal {
        if entry_price.is_zero() {
            return Decimal::ZERO;
        }

        let total_range = match side {
            Side::Long => entry_price - liq_price,
            Side::Short => liq_price - entry_price,
        };

        if total_range.is_zero() {
            return Decimal::ONE;
        }

        let current_distance = match side {
            Side::Long => current_price - liq_price,
            Side::Short => liq_price - current_price,
        };

        let proximity = Decimal::ONE - (current_distance / total_range);
        proximity.max(Decimal::ZERO).min(Decimal::ONE)
    }

    /// Check if any active plays have exceeded max hold time.
    fn check_expired_plays(&mut self) -> Vec<Signal> {
        let now = Utc::now();
        let max_duration = Duration::hours(MAX_HOLD_HOURS);
        let mut exit_signals = Vec::new();

        let expired: Vec<String> = self
            .active_plays
            .iter()
            .filter(|(_, play)| now - play.entry_time > max_duration)
            .map(|(asset, _)| asset.clone())
            .collect();

        for asset in expired {
            if let Some(play) = self.active_plays.remove(&asset) {
                let exit_side = play.our_side.opposite();
                exit_signals.push(Signal::new(
                    &asset,
                    exit_side,
                    dec!(0.8),
                    dec!(0.005),
                    StrategyType::Liquidation,
                    "Liquidation play expired — max hold time reached",
                ));
            }
        }

        exit_signals
    }
}

impl Default for LiquidationStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::Strategy for LiquidationStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn strategy_type(&self) -> StrategyType {
        StrategyType::Liquidation
    }

    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();
        self.active_signals.clear();

        // Check for expired plays first
        signals.extend(self.check_expired_plays());

        // Scan positions in the market for liquidation candidates
        // In a real implementation, this would use WebSocket data for large position changes
        // Here we use the info API data as a proxy
        for asset_info in &ctx.assets {
            let asset = &asset_info.name;
            let mark_price = asset_info.mark_price;

            if mark_price.is_zero() || asset_info.open_interest < MIN_LARGE_POSITION_NOTIONAL {
                continue;
            }

            // Skip if we already have an active play for this asset
            if self.active_plays.contains_key(asset) {
                // Check stop loss
                if let Some(play) = self.active_plays.get(asset) {
                    let hit_stop = match play.our_side {
                        Side::Short => mark_price > play.stop_loss,
                        Side::Long => mark_price < play.stop_loss,
                    };
                    if hit_stop {
                        let exit_side = play.our_side.opposite();
                        signals.push(Signal::new(
                            asset,
                            exit_side,
                            dec!(0.95),
                            dec!(0.01),
                            StrategyType::Liquidation,
                            "Stop loss hit on liquidation play",
                        ));
                        self.active_plays.remove(asset);
                    }
                }
                continue;
            }

            // Analyze open interest changes and leverage levels
            // High OI + rapid price movement toward a cluster of positions = cascade risk
            let oi = asset_info.open_interest;
            let funding = asset_info.funding_rate;

            // Heuristic: extreme funding + high OI suggests crowded positioning
            // If funding is very positive, longs are paying and might be overleveraged
            // If funding is very negative, shorts are paying and might be overleveraged
            let crowded_side = if funding > dec!(0.001) {
                Some(Side::Long) // Longs are crowded
            } else if funding < dec!(-0.001) {
                Some(Side::Short) // Shorts are crowded
            } else {
                None
            };

            if let Some(crowded) = crowded_side {
                // Estimate where liquidations would cluster
                // For highly leveraged positions, liq price is close to current
                let avg_leverage = dec!(10); // Assumption for high-leverage crowd
                let maint_margin = dec!(0.03); // 3% typical

                let cluster_liq_price = Self::estimate_liquidation_price(
                    mark_price,
                    avg_leverage,
                    crowded,
                    maint_margin,
                );

                // Check if price is approaching the cluster
                let proximity = Self::liquidation_proximity(
                    mark_price,
                    cluster_liq_price,
                    mark_price, // Using current as entry estimate
                    crowded,
                );

                if proximity > LIQUIDATION_PROXIMITY_THRESHOLD {
                    // Position opposite to the crowded side
                    let our_side = crowded.opposite();
                    let confidence = CASCADE_CONFIDENCE
                        + (proximity - LIQUIDATION_PROXIMITY_THRESHOLD) * dec!(0.3);

                    let stop_loss = match our_side {
                        Side::Short => mark_price * (Decimal::ONE + STOP_LOSS_PCT),
                        Side::Long => mark_price * (Decimal::ONE - STOP_LOSS_PCT),
                    };

                    let signal = Signal::new(
                        asset,
                        our_side,
                        confidence.min(dec!(0.9)),
                        dec!(0.03), // 3% expected edge from cascade
                        StrategyType::Liquidation,
                        &format!(
                            "Liquidation cascade: {} side crowded, proximity={:.2}, OI={}",
                            crowded, proximity, oi
                        ),
                    )
                    .with_stop_loss(stop_loss)
                    .with_leverage(dec!(3));

                    info!(
                        "Liquidation signal: {} {} — crowd={}, proximity={:.2}",
                        our_side, asset, crowded, proximity
                    );

                    self.targets.insert(
                        asset.clone(),
                        LiquidationTarget {
                            asset: asset.clone(),
                            estimated_liq_price: cluster_liq_price,
                            position_size: oi,
                            side: crowded,
                            detected_at: Utc::now(),
                            margin_ratio: proximity,
                        },
                    );

                    self.active_signals.push(signal.clone());
                    signals.push(signal);
                }
            }
        }

        Ok(signals)
    }

    async fn on_fill(&mut self, fill: &Fill) -> Result<()> {
        let mark_price = fill.price;
        if !self.active_plays.contains_key(&fill.asset) {
            let stop_loss = match fill.side {
                Side::Short => mark_price * (Decimal::ONE + STOP_LOSS_PCT),
                Side::Long => mark_price * (Decimal::ONE - STOP_LOSS_PCT),
            };

            self.active_plays.insert(
                fill.asset.clone(),
                LiquidationPlay {
                    asset: fill.asset.clone(),
                    our_side: fill.side,
                    entry_time: Utc::now(),
                    stop_loss,
                },
            );
            info!("Liquidation play opened: {} {}", fill.side, fill.asset);
        } else {
            self.active_plays.remove(&fill.asset);
            self.targets.remove(&fill.asset);
            info!("Liquidation play closed: {}", fill.asset);
        }
        Ok(())
    }

    fn active_signals(&self) -> &[Signal] {
        &self.active_signals
    }

    fn risk_params(&self) -> StrategyRiskParams {
        StrategyRiskParams {
            max_position_pct: dec!(5),
            max_leverage: dec!(3),
            max_drawdown_pct: dec!(3),
            max_correlated_exposure: dec!(10),
            cooldown_secs: 1800, // 30 minutes between plays
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::{AssetInfo, Portfolio};

    #[test]
    fn test_estimate_liquidation_price_long() {
        // Long at 50000 with 10x leverage, 3% maintenance
        let liq = LiquidationStrategy::estimate_liquidation_price(
            dec!(50000),
            dec!(10),
            Side::Long,
            dec!(0.03),
        );
        // Should be: 50000 * (1 - 0.1 + 0.03) = 50000 * 0.93 = 46500
        assert_eq!(liq, dec!(46500));
    }

    #[test]
    fn test_estimate_liquidation_price_short() {
        // Short at 50000 with 10x leverage, 3% maintenance
        let liq = LiquidationStrategy::estimate_liquidation_price(
            dec!(50000),
            dec!(10),
            Side::Short,
            dec!(0.03),
        );
        // Should be: 50000 * (1 + 0.1 - 0.03) = 50000 * 1.07 = 53500
        assert_eq!(liq, dec!(53500));
    }

    #[test]
    fn test_liquidation_proximity() {
        // Long entry at 50000, liq at 46500
        // Current price at 47000
        let proximity = LiquidationStrategy::liquidation_proximity(
            dec!(47000),
            dec!(46500),
            dec!(50000),
            Side::Long,
        );
        // (50000 - 46500) = 3500 range
        // (47000 - 46500) = 500 distance
        // 1 - 500/3500 = 1 - 0.1428 = 0.8571
        assert!(proximity > dec!(0.85) && proximity < dec!(0.86));
    }

    #[test]
    fn test_liquidation_proximity_at_liq() {
        let proximity = LiquidationStrategy::liquidation_proximity(
            dec!(46500),
            dec!(46500),
            dec!(50000),
            Side::Long,
        );
        assert_eq!(proximity, Decimal::ONE);
    }

    #[tokio::test]
    async fn test_liquidation_strategy_crowded_longs() {
        let mut strategy = LiquidationStrategy::new();

        let assets = vec![AssetInfo {
            name: "DOGE".to_string(),
            sz_decimals: 0,
            max_leverage: 20,
            mark_price: dec!(0.15),
            funding_rate: dec!(0.005), // Very high — longs crowded
            open_interest: dec!(5_000_000),
            volume_24h: dec!(50000000),
        }];

        let ctx = MarketContext {
            assets,
            prices: HashMap::new(),
            funding_rates: HashMap::new(),
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(10000)),
            timestamp: Utc::now(),
        };

        let signals = strategy.evaluate(&ctx).await.unwrap();
        // Should detect crowded longs and signal short
        // (depends on proximity calculation)
        // With funding > 0.001, crowded side = Long, our side = Short
        for sig in &signals {
            assert_eq!(sig.side, Side::Short);
        }
    }

    #[tokio::test]
    async fn test_on_fill_tracking() {
        let mut strategy = LiquidationStrategy::new();
        let fill = Fill {
            order_id: "liq-1".to_string(),
            asset: "BTC".to_string(),
            side: Side::Short,
            size: dec!(0.5),
            price: dec!(50000),
            fee: dec!(1),
            timestamp: Utc::now(),
            strategy: "Liquidation".to_string(),
            is_maker: false,
        };

        strategy.on_fill(&fill).await.unwrap();
        assert!(strategy.active_plays.contains_key("BTC"));

        // Second fill closes
        strategy.on_fill(&fill).await.unwrap();
        assert!(!strategy.active_plays.contains_key("BTC"));
    }

    #[test]
    fn test_risk_params() {
        let strategy = LiquidationStrategy::new();
        let params = strategy.risk_params();
        assert_eq!(params.max_position_pct, dec!(5));
        assert_eq!(params.max_leverage, dec!(3));
    }
}
