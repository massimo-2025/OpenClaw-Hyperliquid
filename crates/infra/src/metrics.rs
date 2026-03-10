use hl_core::Portfolio;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

/// Prometheus-style metrics collector.
/// Tracks key trading metrics for monitoring and dashboarding.
pub struct MetricsCollector {
    gauges: Arc<RwLock<HashMap<String, f64>>>,
    counters: Arc<RwLock<HashMap<String, u64>>>,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            gauges: Arc::new(RwLock::new(HashMap::new())),
            counters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Set a gauge value.
    pub async fn set_gauge(&self, name: &str, value: f64) {
        self.gauges.write().await.insert(name.to_string(), value);
    }

    /// Increment a counter.
    pub async fn increment(&self, name: &str) {
        let mut counters = self.counters.write().await;
        *counters.entry(name.to_string()).or_insert(0) += 1;
    }

    /// Increment a counter by a specific amount.
    pub async fn increment_by(&self, name: &str, amount: u64) {
        let mut counters = self.counters.write().await;
        *counters.entry(name.to_string()).or_insert(0) += amount;
    }

    /// Update portfolio metrics from a Portfolio snapshot.
    pub async fn update_portfolio_metrics(&self, portfolio: &Portfolio) {
        let nav = decimal_to_f64(portfolio.nav);
        let leverage = decimal_to_f64(portfolio.leverage);
        let margin_ratio = decimal_to_f64(portfolio.margin_ratio);
        let drawdown = decimal_to_f64(portfolio.drawdown_from_hwm());
        let daily_pnl = decimal_to_f64(portfolio.daily_pnl());
        let unrealized_pnl = decimal_to_f64(portfolio.total_unrealized_pnl);
        let realized_pnl = decimal_to_f64(portfolio.total_realized_pnl);

        self.set_gauge("portfolio_nav", nav).await;
        self.set_gauge("portfolio_leverage", leverage).await;
        self.set_gauge("portfolio_margin_ratio", margin_ratio).await;
        self.set_gauge("portfolio_drawdown_pct", drawdown).await;
        self.set_gauge("portfolio_daily_pnl", daily_pnl).await;
        self.set_gauge("portfolio_unrealized_pnl", unrealized_pnl).await;
        self.set_gauge("portfolio_realized_pnl", realized_pnl).await;
        self.set_gauge("portfolio_positions", portfolio.positions.len() as f64).await;
    }

    /// Export metrics in Prometheus text format.
    pub async fn export_prometheus(&self) -> String {
        let mut output = String::new();

        let gauges = self.gauges.read().await;
        for (name, value) in gauges.iter() {
            let metric_name = name.replace('.', "_").replace('-', "_");
            output.push_str(&format!(
                "# TYPE hl_{metric_name} gauge\nhl_{metric_name} {value}\n"
            ));
        }

        let counters = self.counters.read().await;
        for (name, value) in counters.iter() {
            let metric_name = name.replace('.', "_").replace('-', "_");
            output.push_str(&format!(
                "# TYPE hl_{metric_name} counter\nhl_{metric_name} {value}\n"
            ));
        }

        output
    }

    /// Get a gauge value.
    pub async fn get_gauge(&self, name: &str) -> Option<f64> {
        self.gauges.read().await.get(name).copied()
    }

    /// Get a counter value.
    pub async fn get_counter(&self, name: &str) -> Option<u64> {
        self.counters.read().await.get(name).copied()
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

fn decimal_to_f64(d: Decimal) -> f64 {
    d.to_string().parse().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn test_set_and_get_gauge() {
        let metrics = MetricsCollector::new();
        metrics.set_gauge("test_gauge", 42.5).await;
        assert_eq!(metrics.get_gauge("test_gauge").await, Some(42.5));
    }

    #[tokio::test]
    async fn test_increment_counter() {
        let metrics = MetricsCollector::new();
        metrics.increment("test_counter").await;
        metrics.increment("test_counter").await;
        metrics.increment("test_counter").await;
        assert_eq!(metrics.get_counter("test_counter").await, Some(3));
    }

    #[tokio::test]
    async fn test_portfolio_metrics() {
        let metrics = MetricsCollector::new();
        let portfolio = Portfolio::new(dec!(10000));
        metrics.update_portfolio_metrics(&portfolio).await;

        assert_eq!(metrics.get_gauge("portfolio_nav").await, Some(10000.0));
        assert_eq!(metrics.get_gauge("portfolio_leverage").await, Some(0.0));
    }

    #[tokio::test]
    async fn test_prometheus_export() {
        let metrics = MetricsCollector::new();
        metrics.set_gauge("nav", 10000.0).await;
        metrics.increment("trades").await;

        let output = metrics.export_prometheus().await;
        assert!(output.contains("hl_nav"));
        assert!(output.contains("hl_trades"));
    }

    #[test]
    fn test_decimal_to_f64() {
        assert_eq!(decimal_to_f64(dec!(42.5)), 42.5);
        assert_eq!(decimal_to_f64(dec!(0)), 0.0);
    }
}
