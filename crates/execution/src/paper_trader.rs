use anyhow::Result;
use chrono::Utc;
use hl_core::{Fill, Order, OrderStatus, Portfolio, Position, Side, Signal};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{debug, info};

use crate::position_tracker::PositionTracker;

/// Estimated slippage for paper trading (in basis points).
const ESTIMATED_SLIPPAGE_BPS: Decimal = dec!(5);
/// Maker fee on Hyperliquid.
const MAKER_FEE_BPS: Decimal = dec!(-1.5); // Rebate
/// Taker fee on Hyperliquid.
const TAKER_FEE_BPS: Decimal = dec!(3.5);

/// Paper trading state for persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperTradingState {
    pub starting_balance: Decimal,
    pub nav: Decimal,
    pub cash: Decimal,
    pub positions: Vec<Position>,
    pub total_trades: u64,
    pub total_realized_pnl: Decimal,
    pub high_water_mark: Decimal,
    pub strategy_pnl: HashMap<String, Decimal>,
    pub created_at: String,
    pub updated_at: String,
}

/// Paper trading engine that simulates order execution.
/// Uses current market prices with estimated slippage to fill orders.
/// Tracks PnL per strategy and persists state to JSON.
pub struct PaperTrader {
    position_tracker: PositionTracker,
    strategy_pnl: HashMap<String, Decimal>,
    total_trades: u64,
    starting_balance: Decimal,
    state_file: String,
    /// Current prices for fill simulation.
    current_prices: HashMap<String, Decimal>,
}

impl PaperTrader {
    pub fn new(starting_balance: Decimal) -> Self {
        Self {
            position_tracker: PositionTracker::new(starting_balance),
            strategy_pnl: HashMap::new(),
            total_trades: 0,
            starting_balance,
            state_file: "data/paper-portfolio.json".to_string(),
            current_prices: HashMap::new(),
        }
    }

    pub fn with_state_file(mut self, path: &str) -> Self {
        self.state_file = path.to_string();
        self
    }

    /// Update current prices for fill simulation.
    pub fn update_prices(&mut self, prices: HashMap<String, Decimal>) {
        self.current_prices = prices;
    }

    /// Simulate executing a signal as a paper trade.
    pub async fn execute_signal(&mut self, signal: &Signal) -> Result<Option<Fill>> {
        let price = self
            .current_prices
            .get(&signal.asset)
            .copied()
            .unwrap_or_default();

        if price.is_zero() {
            info!(
                "Paper: No price available for {}, skipping",
                signal.asset
            );
            return Ok(None);
        }

        // Apply slippage
        let slippage = price * ESTIMATED_SLIPPAGE_BPS / dec!(10000);
        let fill_price = match signal.side {
            Side::Long => price + slippage, // Buy at higher price
            Side::Short => price - slippage, // Sell at lower price
        };

        // Calculate fee
        let fee = fill_price * signal.suggested_size * TAKER_FEE_BPS / dec!(10000);
        let fee = fee.abs(); // Fee is always positive cost

        let fill = Fill {
            order_id: format!("paper-{}", uuid::Uuid::new_v4()),
            asset: signal.asset.clone(),
            side: signal.side,
            size: signal.suggested_size,
            price: fill_price,
            fee,
            timestamp: Utc::now(),
            strategy: format!("{}", signal.strategy),
            is_maker: false,
        };

        // Process through position tracker
        self.position_tracker.process_fill(&fill).await?;

        // Update strategy PnL tracking
        *self
            .strategy_pnl
            .entry(fill.strategy.clone())
            .or_insert(Decimal::ZERO) -= fee;

        self.total_trades += 1;

        info!(
            "Paper trade: {} {} {} @ {} (fee: {})",
            fill.side, fill.size, fill.asset, fill.price, fill.fee
        );

        Ok(Some(fill))
    }

    /// Update mark prices for PnL calculation.
    pub async fn update_mark_prices(&self) {
        self.position_tracker
            .update_mark_prices(&self.current_prices)
            .await;
    }

    /// Get current portfolio state.
    pub async fn portfolio(&self) -> Portfolio {
        self.position_tracker.portfolio().await
    }

    /// Get PnL breakdown by strategy.
    pub fn strategy_pnl(&self) -> &HashMap<String, Decimal> {
        &self.strategy_pnl
    }

    /// Get total number of trades executed.
    pub fn total_trades(&self) -> u64 {
        self.total_trades
    }

    /// Get position tracker reference.
    pub fn position_tracker(&self) -> &PositionTracker {
        &self.position_tracker
    }

    /// Persist current state to JSON file.
    pub async fn save_state(&self) -> Result<()> {
        let portfolio = self.portfolio().await;

        let state = PaperTradingState {
            starting_balance: self.starting_balance,
            nav: portfolio.nav,
            cash: portfolio.cash,
            positions: portfolio.positions,
            total_trades: self.total_trades,
            total_realized_pnl: portfolio.total_realized_pnl,
            high_water_mark: portfolio.high_water_mark,
            strategy_pnl: self.strategy_pnl.clone(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };

        let path = Path::new(&self.state_file);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let json = serde_json::to_string_pretty(&state)?;
        tokio::fs::write(&self.state_file, json).await?;

        debug!("Paper trading state saved to {}", self.state_file);
        Ok(())
    }

    /// Load state from JSON file.
    pub async fn load_state(&mut self) -> Result<bool> {
        let path = Path::new(&self.state_file);
        if !path.exists() {
            return Ok(false);
        }

        let json = tokio::fs::read_to_string(&self.state_file).await?;
        let state: PaperTradingState = serde_json::from_str(&json)?;

        self.strategy_pnl = state.strategy_pnl;
        self.total_trades = state.total_trades;

        info!(
            "Paper trading state loaded: NAV={}, trades={}",
            state.nav, state.total_trades
        );
        Ok(true)
    }

    /// Get a formatted summary string.
    pub async fn summary(&self) -> String {
        let portfolio = self.portfolio().await;
        let positions = self.position_tracker.all_positions();

        let mut summary = format!(
            "📊 Paper Trading Summary\n\
             ━━━━━━━━━━━━━━━━━━━━━━━━\n\
             Starting Balance: ${:.2}\n\
             NAV: ${:.2}\n\
             Cash: ${:.2}\n\
             Total P&L: ${:.2} ({:.2}%)\n\
             Daily P&L: ${:.2}\n\
             Leverage: {:.2}x\n\
             Positions: {}\n\
             Total Trades: {}\n\
             High Water Mark: ${:.2}\n\
             Drawdown: {:.2}%\n\n",
            self.starting_balance,
            portfolio.nav,
            portfolio.cash,
            portfolio.nav - self.starting_balance,
            portfolio.daily_return_pct(),
            portfolio.daily_pnl(),
            portfolio.leverage,
            positions.len(),
            self.total_trades,
            portfolio.high_water_mark,
            portfolio.drawdown_from_hwm(),
        );

        if !positions.is_empty() {
            summary.push_str("Positions:\n");
            for pos in &positions {
                summary.push_str(&format!(
                    "  {} {} {}: size={} entry={} mark={} pnl={:.2}\n",
                    pos.side, pos.asset, pos.strategy, pos.size, pos.entry_price,
                    pos.mark_price, pos.total_pnl()
                ));
            }
        }

        if !self.strategy_pnl.is_empty() {
            summary.push_str("\nStrategy P&L:\n");
            for (strategy, pnl) in &self.strategy_pnl {
                summary.push_str(&format!("  {}: ${:.2}\n", strategy, pnl));
            }
        }

        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::StrategyType;

    fn make_signal(asset: &str, side: Side, size: Decimal) -> Signal {
        Signal::new(
            asset,
            side,
            dec!(0.8),
            dec!(0.05),
            StrategyType::FundingArb,
            "test",
        )
        .with_size(size)
        .with_leverage(dec!(2))
    }

    #[tokio::test]
    async fn test_paper_trader_basic_trade() {
        let mut trader = PaperTrader::new(dec!(10000));
        let mut prices = HashMap::new();
        prices.insert("BTC".to_string(), dec!(50000));
        trader.update_prices(prices);

        let signal = make_signal("BTC", Side::Long, dec!(0.1));
        let fill = trader.execute_signal(&signal).await.unwrap();

        assert!(fill.is_some());
        let fill = fill.unwrap();
        assert_eq!(fill.asset, "BTC");
        assert_eq!(fill.side, Side::Long);
        // Price should include slippage
        assert!(fill.price > dec!(50000));
        assert_eq!(trader.total_trades(), 1);
    }

    #[tokio::test]
    async fn test_paper_trader_no_price() {
        let mut trader = PaperTrader::new(dec!(10000));
        // No prices set

        let signal = make_signal("ETH", Side::Long, dec!(5));
        let fill = trader.execute_signal(&signal).await.unwrap();
        assert!(fill.is_none());
    }

    #[tokio::test]
    async fn test_paper_trader_portfolio_updates() {
        let mut trader = PaperTrader::new(dec!(10000));
        let mut prices = HashMap::new();
        prices.insert("ETH".to_string(), dec!(3000));
        trader.update_prices(prices);

        let signal = make_signal("ETH", Side::Long, dec!(1));
        trader.execute_signal(&signal).await.unwrap();

        // Update prices up
        let mut new_prices = HashMap::new();
        new_prices.insert("ETH".to_string(), dec!(3100));
        trader.update_prices(new_prices);
        trader.update_mark_prices().await;

        let portfolio = trader.portfolio().await;
        // Should have unrealized profit
        assert!(portfolio.total_unrealized_pnl > Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_paper_trader_strategy_pnl() {
        let mut trader = PaperTrader::new(dec!(10000));
        let mut prices = HashMap::new();
        prices.insert("SOL".to_string(), dec!(100));
        trader.update_prices(prices);

        let signal = make_signal("SOL", Side::Long, dec!(10));
        trader.execute_signal(&signal).await.unwrap();

        // Should have strategy PnL entry (from fees)
        let pnl = trader.strategy_pnl();
        assert!(pnl.contains_key("FundingArb"));
    }

    #[tokio::test]
    async fn test_paper_trader_summary() {
        let trader = PaperTrader::new(dec!(10000));
        let summary = trader.summary().await;
        assert!(summary.contains("Paper Trading Summary"));
        assert!(summary.contains("10000"));
    }

    #[test]
    fn test_paper_trading_state_serialization() {
        let state = PaperTradingState {
            starting_balance: dec!(10000),
            nav: dec!(10500),
            cash: dec!(5000),
            positions: Vec::new(),
            total_trades: 42,
            total_realized_pnl: dec!(500),
            high_water_mark: dec!(10500),
            strategy_pnl: HashMap::new(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T12:00:00Z".to_string(),
        };

        let json = serde_json::to_string_pretty(&state).unwrap();
        assert!(json.contains("10500"));

        let deser: PaperTradingState = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.total_trades, 42);
    }
}
