use rust_decimal::prelude::Signed;
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use hl_core::{Fill, MarketContext, Side, Signal, StrategyRiskParams, StrategyType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info, warn};

/// Minimum spread in basis points.
const MIN_SPREAD_BPS: Decimal = dec!(5);
/// Maximum inventory as fraction of NAV before kill switch.
const MAX_INVENTORY_PCT: Decimal = dec!(10);
/// Inventory skew factor: how much to shift quotes per unit of inventory.
const INVENTORY_SKEW_FACTOR: Decimal = dec!(0.3);
/// Maker rebate on Hyperliquid.
const MAKER_REBATE_BPS: Decimal = dec!(1.5);
/// Maximum number of markets to make simultaneously.
const MAX_MARKETS: usize = 5;

/// Current inventory tracking per asset.
#[derive(Debug, Clone)]
struct InventoryState {
    net_position: Decimal,
    avg_entry_price: Decimal,
    realized_pnl: Decimal,
    num_trades: u64,
    volume_traded: Decimal,
}

impl InventoryState {
    fn new() -> Self {
        Self {
            net_position: Decimal::ZERO,
            avg_entry_price: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            num_trades: 0,
            volume_traded: Decimal::ZERO,
        }
    }
}

/// Two-Sided Market Making Strategy.
///
/// Places bid and ask quotes around the mid price with a spread that
/// adapts to volatility and inventory levels. Profits from the bid-ask
/// spread plus maker rebates. Includes inventory management to prevent
/// accumulating large directional exposure.
pub struct MarketMakerStrategy {
    name: String,
    active_signals: Vec<Signal>,
    inventories: HashMap<String, InventoryState>,
    /// Assets we're actively making markets in.
    active_markets: Vec<String>,
    /// Recent volatility estimate per asset (realized vol in bps).
    volatility_estimates: HashMap<String, Decimal>,
}

impl MarketMakerStrategy {
    pub fn new() -> Self {
        Self {
            name: "MarketMaker".to_string(),
            active_signals: Vec::new(),
            inventories: HashMap::new(),
            active_markets: Vec::new(),
            volatility_estimates: HashMap::new(),
        }
    }

    /// Select which markets to make based on volume, spread, and volatility.
    fn select_markets(&self, ctx: &MarketContext) -> Vec<String> {
        let mut candidates: Vec<_> = ctx
            .assets
            .iter()
            .filter(|a| {
                a.volume_24h > dec!(1_000_000) // Minimum volume
                    && a.mark_price > Decimal::ZERO
            })
            .map(|a| (a.name.clone(), a.volume_24h))
            .collect();

        // Sort by volume descending
        candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        candidates
            .into_iter()
            .take(MAX_MARKETS)
            .map(|(name, _)| name)
            .collect()
    }

    /// Compute the optimal spread for an asset based on volatility and inventory.
    fn compute_spread(&self, asset: &str, base_vol: Decimal) -> Decimal {
        // Volatility-adjusted spread: higher vol = wider spread
        let vol_spread = base_vol * dec!(2);
        let spread = vol_spread.max(MIN_SPREAD_BPS);

        // Widen spread if we have inventory on one side
        let inventory_adjustment = if let Some(inv) = self.inventories.get(asset) {
            inv.net_position.abs() * INVENTORY_SKEW_FACTOR
        } else {
            Decimal::ZERO
        };

        spread + inventory_adjustment
    }

    /// Compute quote prices with inventory skew.
    fn compute_quotes(
        &self,
        asset: &str,
        mid_price: Decimal,
        spread_bps: Decimal,
    ) -> (Decimal, Decimal) {
        let half_spread = mid_price * spread_bps / dec!(20000); // bps to price

        // Skew quotes away from inventory
        let skew = if let Some(inv) = self.inventories.get(asset) {
            // If long inventory, lower bid price (less eager to buy more)
            // and lower ask price (more eager to sell)
            inv.net_position * INVENTORY_SKEW_FACTOR * mid_price / dec!(10000)
        } else {
            Decimal::ZERO
        };

        let bid = mid_price - half_spread - skew;
        let ask = mid_price + half_spread - skew;
        (bid, ask)
    }

    /// Estimate realized volatility from recent price data (in bps).
    fn estimate_volatility(&self, _asset: &str, ctx: &MarketContext) -> Decimal {
        // Simple proxy: use orderbook spread as volatility estimate
        // In production, use rolling window of returns
        dec!(10) // Default 10 bps if no data
    }

    /// Check if inventory exceeds kill switch threshold.
    fn inventory_exceeded(&self, asset: &str, nav: Decimal) -> bool {
        if let Some(inv) = self.inventories.get(asset) {
            if nav.is_zero() {
                return false;
            }
            let inv_pct = inv.net_position.abs() * dec!(100) / nav;
            inv_pct > MAX_INVENTORY_PCT
        } else {
            false
        }
    }
}

impl Default for MarketMakerStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::Strategy for MarketMakerStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn strategy_type(&self) -> StrategyType {
        StrategyType::MarketMaker
    }

    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();
        self.active_signals.clear();

        // Select markets to make
        let markets = self.select_markets(ctx);

        for asset in &markets {
            let mid_price = ctx.prices.get(asset).copied().unwrap_or_default();
            if mid_price.is_zero() {
                continue;
            }

            // Check inventory kill switch
            if self.inventory_exceeded(asset, ctx.portfolio.nav) {
                warn!("MarketMaker: Inventory exceeded for {asset}, skipping");
                continue;
            }

            // Estimate volatility
            let vol = self.estimate_volatility(asset, ctx);
            self.volatility_estimates.insert(asset.clone(), vol);

            // Compute spread and quotes
            let spread = self.compute_spread(asset, vol);
            let (bid_price, ask_price) = self.compute_quotes(asset, mid_price, spread);

            // Generate bid signal
            let bid_signal = Signal::new(
                asset,
                Side::Long,
                dec!(0.7),
                MAKER_REBATE_BPS / dec!(10000) + spread / dec!(20000), // Edge = half spread + rebate
                StrategyType::MarketMaker,
                &format!(
                    "MM bid: mid={}, bid={}, spread={:.1}bps",
                    mid_price, bid_price, spread
                ),
            )
            .with_leverage(dec!(2));

            // Generate ask signal
            let ask_signal = Signal::new(
                asset,
                Side::Short,
                dec!(0.7),
                MAKER_REBATE_BPS / dec!(10000) + spread / dec!(20000),
                StrategyType::MarketMaker,
                &format!(
                    "MM ask: mid={}, ask={}, spread={:.1}bps",
                    mid_price, ask_price, spread
                ),
            )
            .with_leverage(dec!(2));

            self.active_signals.push(bid_signal.clone());
            self.active_signals.push(ask_signal.clone());
            signals.push(bid_signal);
            signals.push(ask_signal);
        }

        if !signals.is_empty() {
            debug!("MarketMaker: Generated {} quote signals", signals.len());
        }

        Ok(signals)
    }

    async fn on_fill(&mut self, fill: &Fill) -> Result<()> {
        let inv = self
            .inventories
            .entry(fill.asset.clone())
            .or_insert_with(InventoryState::new);

        let direction = fill.side.sign();
        let old_position = inv.net_position;
        inv.net_position += direction * fill.size;
        inv.num_trades += 1;
        inv.volume_traded += fill.size * fill.price;

        // Update average entry price
        if inv.net_position.abs() > old_position.abs() {
            // Adding to position
            let total_cost = inv.avg_entry_price * old_position.abs() + fill.price * fill.size;
            if !inv.net_position.is_zero() {
                inv.avg_entry_price = total_cost / inv.net_position.abs();
            }
        } else if inv.net_position.is_zero() || inv.net_position.signum() != old_position.signum() {
            // Position reduced to zero or flipped
            let pnl = direction * fill.size * (fill.price - inv.avg_entry_price);
            inv.realized_pnl += pnl;
            if !inv.net_position.is_zero() {
                inv.avg_entry_price = fill.price;
            }
        }

        info!(
            "MarketMaker fill: {} {} {} @ {} | net_pos={}",
            fill.side, fill.size, fill.asset, fill.price, inv.net_position
        );

        Ok(())
    }

    fn active_signals(&self) -> &[Signal] {
        &self.active_signals
    }

    fn risk_params(&self) -> StrategyRiskParams {
        StrategyRiskParams {
            max_position_pct: dec!(10),
            max_leverage: dec!(3),
            max_drawdown_pct: dec!(5),
            max_correlated_exposure: dec!(20),
            cooldown_secs: 0, // MM needs to be always-on
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::{AssetInfo, Portfolio};

    fn make_mm_context() -> MarketContext {
        let mut prices = HashMap::new();
        prices.insert("BTC".to_string(), dec!(50000));
        prices.insert("ETH".to_string(), dec!(3000));

        let assets = vec![
            AssetInfo {
                name: "BTC".to_string(),
                sz_decimals: 4,
                max_leverage: 50,
                mark_price: dec!(50000),
                oracle_price: dec!(50000),
                funding_rate: dec!(0.0001),
                open_interest: dec!(100000000),
                volume_24h: dec!(500000000),
                premium: Decimal::ZERO,
            },
            AssetInfo {
                name: "ETH".to_string(),
                sz_decimals: 3,
                max_leverage: 50,
                mark_price: dec!(3000),
                oracle_price: dec!(3000),
                funding_rate: dec!(0.0002),
                open_interest: dec!(50000000),
                volume_24h: dec!(200000000),
                premium: Decimal::ZERO,
            },
        ];

        MarketContext {
            assets,
            prices,
            funding_rates: HashMap::new(),
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(100000)),
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_market_maker_generates_two_sided_quotes() {
        let mut strategy = MarketMakerStrategy::new();
        let ctx = make_mm_context();
        let signals = strategy.evaluate(&ctx).await.unwrap();

        // Should generate bid + ask for each market
        assert!(signals.len() >= 2);

        // Check we have both sides
        let has_long = signals.iter().any(|s| s.side == Side::Long);
        let has_short = signals.iter().any(|s| s.side == Side::Short);
        assert!(has_long);
        assert!(has_short);
    }

    #[test]
    fn test_compute_spread_minimum() {
        let strategy = MarketMakerStrategy::new();
        let spread = strategy.compute_spread("BTC", dec!(1)); // Very low vol
        assert!(spread >= MIN_SPREAD_BPS);
    }

    #[test]
    fn test_compute_spread_with_inventory() {
        let mut strategy = MarketMakerStrategy::new();
        strategy.inventories.insert(
            "BTC".to_string(),
            InventoryState {
                net_position: dec!(10),
                avg_entry_price: dec!(50000),
                realized_pnl: Decimal::ZERO,
                num_trades: 5,
                volume_traded: dec!(500000),
            },
        );

        let spread_no_inv = MarketMakerStrategy::new().compute_spread("BTC", dec!(10));
        let spread_with_inv = strategy.compute_spread("BTC", dec!(10));
        assert!(spread_with_inv > spread_no_inv);
    }

    #[test]
    fn test_compute_quotes_symmetric() {
        let strategy = MarketMakerStrategy::new();
        let (bid, ask) = strategy.compute_quotes("BTC", dec!(50000), dec!(10));
        assert!(bid < dec!(50000));
        assert!(ask > dec!(50000));
        // Spread should be symmetric around mid
        let mid = (bid + ask) / dec!(2);
        assert!((mid - dec!(50000)).abs() < dec!(1));
    }

    #[tokio::test]
    async fn test_on_fill_updates_inventory() {
        let mut strategy = MarketMakerStrategy::new();
        let fill = Fill {
            order_id: "mm-1".to_string(),
            asset: "BTC".to_string(),
            side: Side::Long,
            size: dec!(0.1),
            price: dec!(50000),
            fee: dec!(0.5),
            timestamp: Utc::now(),
            strategy: "MarketMaker".to_string(),
            is_maker: true,
        };

        strategy.on_fill(&fill).await.unwrap();
        let inv = strategy.inventories.get("BTC").unwrap();
        assert_eq!(inv.net_position, dec!(0.1));
        assert_eq!(inv.num_trades, 1);
    }

    #[test]
    fn test_inventory_exceeded() {
        let mut strategy = MarketMakerStrategy::new();
        strategy.inventories.insert(
            "BTC".to_string(),
            InventoryState {
                net_position: dec!(15), // 15% of 100 NAV
                avg_entry_price: dec!(50000),
                realized_pnl: Decimal::ZERO,
                num_trades: 10,
                volume_traded: dec!(750000),
            },
        );

        assert!(strategy.inventory_exceeded("BTC", dec!(100)));
        assert!(!strategy.inventory_exceeded("BTC", dec!(1000)));
    }
}
