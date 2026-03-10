use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use hl_core::{Fill, Portfolio, Position, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tracing::{debug, info};

/// Real-time position and PnL tracking across all strategies.
pub struct PositionTracker {
    positions: Arc<DashMap<String, Position>>,
    portfolio: Arc<tokio::sync::RwLock<Portfolio>>,
}

impl PositionTracker {
    pub fn new(starting_balance: Decimal) -> Self {
        Self {
            positions: Arc::new(DashMap::new()),
            portfolio: Arc::new(tokio::sync::RwLock::new(Portfolio::new(starting_balance))),
        }
    }

    /// Process a fill and update positions accordingly.
    pub async fn process_fill(&self, fill: &Fill) -> Result<()> {
        let asset = &fill.asset;
        let mut portfolio = self.portfolio.write().await;

        if let Some(mut pos) = self.positions.get_mut(asset) {
            let position = pos.value_mut();

            // Check if adding to or reducing position
            if position.side == fill.side {
                // Adding to position — update average entry
                let old_cost = position.entry_price * position.size;
                let new_cost = fill.price * fill.size;
                let new_size = position.size + fill.size;
                position.entry_price = (old_cost + new_cost) / new_size;
                position.size = new_size;
                position.margin_used += fill.price * fill.size / position.leverage;
            } else {
                // Reducing position
                if fill.size >= position.size {
                    // Closing or flipping
                    let realized = position.side.sign()
                        * position.size
                        * (fill.price - position.entry_price);
                    position.realized_pnl += realized;
                    portfolio.cash += realized - fill.fee;
                    portfolio.total_realized_pnl += realized;

                    if fill.size > position.size {
                        // Flipping direction
                        let remaining = fill.size - position.size;
                        position.side = fill.side;
                        position.size = remaining;
                        position.entry_price = fill.price;
                        position.margin_used = fill.price * remaining / position.leverage;
                        position.unrealized_pnl = Decimal::ZERO;
                    } else {
                        // Fully closed — remove position
                        drop(pos);
                        self.positions.remove(asset);
                        portfolio.positions.retain(|p| p.asset != *asset);
                        portfolio.recalculate();
                        info!(
                            "Position closed: {} — realized PnL: {}",
                            asset, realized
                        );
                        return Ok(());
                    }
                } else {
                    // Partial close
                    let realized = position.side.sign()
                        * fill.size
                        * (fill.price - position.entry_price);
                    position.realized_pnl += realized;
                    position.size -= fill.size;
                    position.margin_used =
                        position.entry_price * position.size / position.leverage;
                    portfolio.cash += realized - fill.fee;
                    portfolio.total_realized_pnl += realized;
                }
            }

            position.updated_at = Utc::now();
            info!(
                "Position updated: {} {} {} @ {} (size: {})",
                fill.side, fill.size, asset, fill.price, position.size
            );
        } else {
            // New position
            let leverage = dec!(1); // Default, will be adjusted by risk engine
            let margin_used = fill.price * fill.size / leverage;

            let position = Position {
                asset: asset.clone(),
                side: fill.side,
                size: fill.size,
                entry_price: fill.price,
                mark_price: fill.price,
                liquidation_price: None,
                unrealized_pnl: Decimal::ZERO,
                realized_pnl: Decimal::ZERO,
                leverage,
                margin_used,
                strategy: fill.strategy.clone(),
                opened_at: Utc::now(),
                updated_at: Utc::now(),
                cumulative_funding: Decimal::ZERO,
            };

            self.positions.insert(asset.clone(), position.clone());
            portfolio.positions.push(position);
            portfolio.cash -= margin_used + fill.fee;

            info!(
                "New position: {} {} @ {} (leverage: {}x)",
                fill.side, asset, fill.price, leverage
            );
        }

        // Sync portfolio positions from DashMap
        portfolio.positions = self
            .positions
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        portfolio.recalculate();

        Ok(())
    }

    /// Update mark prices for all positions.
    pub async fn update_mark_prices(
        &self,
        prices: &std::collections::HashMap<String, Decimal>,
    ) {
        for mut entry in self.positions.iter_mut() {
            if let Some(&price) = prices.get(entry.key()) {
                entry.value_mut().update_mark_price(price);
            }
        }

        let mut portfolio = self.portfolio.write().await;
        portfolio.positions = self
            .positions
            .iter()
            .map(|entry| entry.value().clone())
            .collect();
        portfolio.recalculate();
    }

    /// Get current portfolio snapshot.
    pub async fn portfolio(&self) -> Portfolio {
        self.portfolio.read().await.clone()
    }

    /// Get a specific position.
    pub fn get_position(&self, asset: &str) -> Option<Position> {
        self.positions.get(asset).map(|p| p.value().clone())
    }

    /// Get all positions.
    pub fn all_positions(&self) -> Vec<Position> {
        self.positions
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get number of open positions.
    pub fn position_count(&self) -> usize {
        self.positions.len()
    }

    /// Check if we have a position in the given asset.
    pub fn has_position(&self, asset: &str) -> bool {
        self.positions.contains_key(asset)
    }

    /// Reset daily PnL tracking (call at start of each day).
    pub async fn reset_daily(&self) {
        let mut portfolio = self.portfolio.write().await;
        portfolio.daily_starting_nav = portfolio.nav;
    }

    /// Apply funding payment to a position.
    pub async fn apply_funding(&self, asset: &str, amount: Decimal) {
        if let Some(mut pos) = self.positions.get_mut(asset) {
            pos.value_mut().cumulative_funding += amount;
            let mut portfolio = self.portfolio.write().await;
            portfolio.cash += amount;
            debug!("Funding applied to {}: {}", asset, amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fill(asset: &str, side: Side, size: Decimal, price: Decimal) -> Fill {
        Fill {
            order_id: format!("test-{}", uuid::Uuid::new_v4()),
            asset: asset.to_string(),
            side,
            size,
            price,
            fee: dec!(0.1),
            timestamp: Utc::now(),
            strategy: "test".to_string(),
            is_maker: false,
        }
    }

    #[tokio::test]
    async fn test_new_position() {
        let tracker = PositionTracker::new(dec!(10000));
        let fill = make_fill("BTC", Side::Long, dec!(0.1), dec!(50000));
        tracker.process_fill(&fill).await.unwrap();

        assert!(tracker.has_position("BTC"));
        assert_eq!(tracker.position_count(), 1);

        let pos = tracker.get_position("BTC").unwrap();
        assert_eq!(pos.side, Side::Long);
        assert_eq!(pos.size, dec!(0.1));
        assert_eq!(pos.entry_price, dec!(50000));
    }

    #[tokio::test]
    async fn test_add_to_position() {
        let tracker = PositionTracker::new(dec!(100000));
        let fill1 = make_fill("ETH", Side::Long, dec!(5), dec!(3000));
        let fill2 = make_fill("ETH", Side::Long, dec!(5), dec!(3200));

        tracker.process_fill(&fill1).await.unwrap();
        tracker.process_fill(&fill2).await.unwrap();

        let pos = tracker.get_position("ETH").unwrap();
        assert_eq!(pos.size, dec!(10));
        // Average entry: (3000*5 + 3200*5) / 10 = 3100
        assert_eq!(pos.entry_price, dec!(3100));
    }

    #[tokio::test]
    async fn test_close_position() {
        let tracker = PositionTracker::new(dec!(100000));
        let open = make_fill("BTC", Side::Long, dec!(1), dec!(50000));
        tracker.process_fill(&open).await.unwrap();

        let close = make_fill("BTC", Side::Short, dec!(1), dec!(51000));
        tracker.process_fill(&close).await.unwrap();

        assert!(!tracker.has_position("BTC"));
        assert_eq!(tracker.position_count(), 0);
    }

    #[tokio::test]
    async fn test_partial_close() {
        let tracker = PositionTracker::new(dec!(100000));
        let open = make_fill("SOL", Side::Long, dec!(100), dec!(100));
        tracker.process_fill(&open).await.unwrap();

        let partial = make_fill("SOL", Side::Short, dec!(50), dec!(110));
        tracker.process_fill(&partial).await.unwrap();

        let pos = tracker.get_position("SOL").unwrap();
        assert_eq!(pos.size, dec!(50));
    }

    #[tokio::test]
    async fn test_update_mark_prices() {
        let tracker = PositionTracker::new(dec!(100000));
        let fill = make_fill("BTC", Side::Long, dec!(1), dec!(50000));
        tracker.process_fill(&fill).await.unwrap();

        let mut prices = std::collections::HashMap::new();
        prices.insert("BTC".to_string(), dec!(51000));
        tracker.update_mark_prices(&prices).await;

        let pos = tracker.get_position("BTC").unwrap();
        assert_eq!(pos.mark_price, dec!(51000));
        assert_eq!(pos.unrealized_pnl, dec!(1000));
    }

    #[tokio::test]
    async fn test_portfolio_after_trades() {
        let tracker = PositionTracker::new(dec!(10000));
        let fill = make_fill("ETH", Side::Long, dec!(1), dec!(3000));
        tracker.process_fill(&fill).await.unwrap();

        let portfolio = tracker.portfolio().await;
        assert_eq!(portfolio.positions.len(), 1);
        assert!(portfolio.nav > Decimal::ZERO);
    }

    #[tokio::test]
    async fn test_apply_funding() {
        let tracker = PositionTracker::new(dec!(10000));
        let fill = make_fill("BTC", Side::Short, dec!(1), dec!(50000));
        tracker.process_fill(&fill).await.unwrap();

        tracker.apply_funding("BTC", dec!(5)).await;

        let pos = tracker.get_position("BTC").unwrap();
        assert_eq!(pos.cumulative_funding, dec!(5));
    }
}
