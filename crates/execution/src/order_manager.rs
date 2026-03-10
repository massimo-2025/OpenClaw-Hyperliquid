use anyhow::Result;
use chrono::Utc;
use dashmap::DashMap;
use hl_core::{Fill, Order, OrderStatus, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Manages the lifecycle of all orders across strategies.
/// Tracks open orders, handles partial fills, and monitors slippage.
pub struct OrderManager {
    /// All orders by order ID.
    orders: Arc<DashMap<String, Order>>,
    /// Slippage tracking: (expected_price, actual_price).
    slippage_records: Arc<DashMap<String, (Decimal, Decimal)>>,
    /// Fill callbacks per strategy.
    pending_fills: Arc<tokio::sync::Mutex<Vec<Fill>>>,
}

impl OrderManager {
    pub fn new() -> Self {
        Self {
            orders: Arc::new(DashMap::new()),
            slippage_records: Arc::new(DashMap::new()),
            pending_fills: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Register a new order for tracking.
    pub fn track_order(&self, order: Order) {
        info!(
            "Tracking order: {} {} {} {} @ {:?}",
            order.client_id, order.side, order.size, order.asset, order.price
        );
        self.orders.insert(order.client_id.clone(), order);
    }

    /// Process a fill notification from the exchange.
    pub async fn process_fill(&self, fill: Fill) -> Result<()> {
        // Update the order state
        if let Some(mut order) = self.orders.get_mut(&fill.order_id) {
            order.filled_size += fill.size;
            order.avg_fill_price = Some(
                if let Some(prev_avg) = order.avg_fill_price {
                    let prev_filled = order.filled_size - fill.size;
                    (prev_avg * prev_filled + fill.price * fill.size) / order.filled_size
                } else {
                    fill.price
                },
            );

            if order.filled_size >= order.size {
                order.status = OrderStatus::Filled;
            } else {
                order.status = OrderStatus::PartiallyFilled;
            }
            order.updated_at = Utc::now();

            // Track slippage
            if let Some(expected_price) = order.price {
                self.slippage_records.insert(
                    fill.order_id.clone(),
                    (expected_price, fill.price),
                );
            }

            info!(
                "Fill processed: {} — {}/{} filled @ {}",
                fill.order_id, order.filled_size, order.size, fill.price
            );
        }

        // Queue for strategy notification
        self.pending_fills.lock().await.push(fill);

        Ok(())
    }

    /// Drain pending fills for strategy notification.
    pub async fn drain_fills(&self) -> Vec<Fill> {
        let mut fills = self.pending_fills.lock().await;
        std::mem::take(&mut *fills)
    }

    /// Get an order by ID.
    pub fn get_order(&self, order_id: &str) -> Option<Order> {
        self.orders.get(order_id).map(|o| o.value().clone())
    }

    /// Get all open (non-terminal) orders.
    pub fn open_orders(&self) -> Vec<Order> {
        self.orders
            .iter()
            .filter(|entry| !entry.value().is_complete())
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get orders for a specific strategy.
    pub fn orders_for_strategy(&self, strategy: &str) -> Vec<Order> {
        self.orders
            .iter()
            .filter(|entry| entry.value().strategy == strategy)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Get orders for a specific asset.
    pub fn orders_for_asset(&self, asset: &str) -> Vec<Order> {
        self.orders
            .iter()
            .filter(|entry| entry.value().asset == asset)
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Cancel an order (mark as cancelled locally).
    pub fn mark_cancelled(&self, order_id: &str) -> bool {
        if let Some(mut order) = self.orders.get_mut(order_id) {
            order.status = OrderStatus::Cancelled;
            order.updated_at = Utc::now();
            info!("Order cancelled: {}", order_id);
            true
        } else {
            warn!("Order not found for cancellation: {}", order_id);
            false
        }
    }

    /// Cancel all open orders for an asset.
    pub fn cancel_all_for_asset(&self, asset: &str) -> usize {
        let mut count = 0;
        for mut entry in self.orders.iter_mut() {
            let order = entry.value_mut();
            if order.asset == asset && !order.is_complete() {
                order.status = OrderStatus::Cancelled;
                order.updated_at = Utc::now();
                count += 1;
            }
        }
        info!("Cancelled {} orders for {}", count, asset);
        count
    }

    /// Get average slippage across all recorded fills.
    pub fn average_slippage(&self) -> Decimal {
        let records: Vec<_> = self
            .slippage_records
            .iter()
            .map(|entry| {
                let (expected, actual) = entry.value();
                if expected.is_zero() {
                    Decimal::ZERO
                } else {
                    ((*actual - *expected) / *expected).abs()
                }
            })
            .collect();

        if records.is_empty() {
            return Decimal::ZERO;
        }

        let sum: Decimal = records.iter().sum();
        sum / Decimal::from(records.len())
    }

    /// Get total number of tracked orders.
    pub fn total_orders(&self) -> usize {
        self.orders.len()
    }

    /// Get number of open orders.
    pub fn open_order_count(&self) -> usize {
        self.orders
            .iter()
            .filter(|entry| !entry.value().is_complete())
            .count()
    }

    /// Clean up completed orders older than the given duration.
    pub fn cleanup_old_orders(&self, max_age: std::time::Duration) {
        let cutoff = Utc::now() - chrono::Duration::from_std(max_age).unwrap_or_default();
        self.orders.retain(|_, order| {
            !(order.is_complete() && order.updated_at < cutoff)
        });
    }
}

impl Default for OrderManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::OrderType;

    fn make_test_order(asset: &str, side: Side) -> Order {
        Order::new_market(asset, side, dec!(1), "test")
    }

    #[test]
    fn test_track_order() {
        let mgr = OrderManager::new();
        let order = make_test_order("BTC", Side::Long);
        let id = order.client_id.clone();
        mgr.track_order(order);

        assert_eq!(mgr.total_orders(), 1);
        let retrieved = mgr.get_order(&id).unwrap();
        assert_eq!(retrieved.asset, "BTC");
    }

    #[tokio::test]
    async fn test_process_fill() {
        let mgr = OrderManager::new();
        let order = make_test_order("ETH", Side::Long);
        let order_id = order.client_id.clone();
        mgr.track_order(order);

        let fill = Fill {
            order_id: order_id.clone(),
            asset: "ETH".to_string(),
            side: Side::Long,
            size: dec!(1),
            price: dec!(3000),
            fee: dec!(0.5),
            timestamp: Utc::now(),
            strategy: "test".to_string(),
            is_maker: false,
        };

        mgr.process_fill(fill).await.unwrap();

        let updated = mgr.get_order(&order_id).unwrap();
        assert_eq!(updated.status, OrderStatus::Filled);
        assert_eq!(updated.avg_fill_price, Some(dec!(3000)));
    }

    #[tokio::test]
    async fn test_partial_fill() {
        let mgr = OrderManager::new();
        let mut order = Order::new_market("BTC", Side::Long, dec!(2), "test");
        let order_id = order.client_id.clone();
        mgr.track_order(order);

        // First partial fill
        let fill1 = Fill {
            order_id: order_id.clone(),
            asset: "BTC".to_string(),
            side: Side::Long,
            size: dec!(1),
            price: dec!(50000),
            fee: dec!(0.5),
            timestamp: Utc::now(),
            strategy: "test".to_string(),
            is_maker: false,
        };
        mgr.process_fill(fill1).await.unwrap();

        let after_partial = mgr.get_order(&order_id).unwrap();
        assert_eq!(after_partial.status, OrderStatus::PartiallyFilled);
        assert_eq!(after_partial.filled_size, dec!(1));
    }

    #[test]
    fn test_open_orders() {
        let mgr = OrderManager::new();
        mgr.track_order(make_test_order("BTC", Side::Long));
        mgr.track_order(make_test_order("ETH", Side::Short));

        assert_eq!(mgr.open_orders().len(), 2);
        assert_eq!(mgr.open_order_count(), 2);
    }

    #[test]
    fn test_cancel_order() {
        let mgr = OrderManager::new();
        let order = make_test_order("BTC", Side::Long);
        let id = order.client_id.clone();
        mgr.track_order(order);

        assert!(mgr.mark_cancelled(&id));
        let cancelled = mgr.get_order(&id).unwrap();
        assert_eq!(cancelled.status, OrderStatus::Cancelled);
        assert_eq!(mgr.open_order_count(), 0);
    }

    #[test]
    fn test_cancel_all_for_asset() {
        let mgr = OrderManager::new();
        mgr.track_order(make_test_order("BTC", Side::Long));
        mgr.track_order(make_test_order("BTC", Side::Short));
        mgr.track_order(make_test_order("ETH", Side::Long));

        let count = mgr.cancel_all_for_asset("BTC");
        assert_eq!(count, 2);
        assert_eq!(mgr.open_order_count(), 1);
    }

    #[test]
    fn test_orders_for_strategy() {
        let mgr = OrderManager::new();
        let mut o1 = make_test_order("BTC", Side::Long);
        o1.strategy = "funding_arb".to_string();
        let mut o2 = make_test_order("ETH", Side::Short);
        o2.strategy = "momentum".to_string();
        mgr.track_order(o1);
        mgr.track_order(o2);

        let funding_orders = mgr.orders_for_strategy("funding_arb");
        assert_eq!(funding_orders.len(), 1);
    }

    #[tokio::test]
    async fn test_drain_fills() {
        let mgr = OrderManager::new();
        let order = make_test_order("BTC", Side::Long);
        let order_id = order.client_id.clone();
        mgr.track_order(order);

        let fill = Fill {
            order_id: order_id.clone(),
            asset: "BTC".to_string(),
            side: Side::Long,
            size: dec!(1),
            price: dec!(50000),
            fee: dec!(0.5),
            timestamp: Utc::now(),
            strategy: "test".to_string(),
            is_maker: false,
        };
        mgr.process_fill(fill).await.unwrap();

        let fills = mgr.drain_fills().await;
        assert_eq!(fills.len(), 1);

        // Should be empty after drain
        let fills2 = mgr.drain_fills().await;
        assert!(fills2.is_empty());
    }
}
