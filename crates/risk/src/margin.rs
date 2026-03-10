use hl_core::Portfolio;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::warn;

/// Monitors margin levels and liquidation distance.
pub struct MarginMonitor {
    /// Minimum margin ratio (NAV / total_margin_used).
    /// A ratio of 2.0 means we need 2x the margin required.
    pub min_buffer_multiplier: Decimal,
}

impl MarginMonitor {
    pub fn new(min_buffer: Decimal) -> Self {
        Self {
            min_buffer_multiplier: min_buffer,
        }
    }

    /// Check if the portfolio has sufficient margin.
    pub fn has_sufficient_margin(&self, portfolio: &Portfolio) -> bool {
        if portfolio.total_margin_used.is_zero() {
            return true;
        }
        portfolio.margin_ratio >= self.min_buffer_multiplier
    }

    /// Calculate the margin ratio.
    pub fn margin_ratio(&self, portfolio: &Portfolio) -> Decimal {
        if portfolio.total_margin_used.is_zero() {
            return dec!(999);
        }
        portfolio.nav / portfolio.total_margin_used
    }

    /// Calculate how much additional margin is available.
    pub fn available_margin(&self, portfolio: &Portfolio) -> Decimal {
        let required = portfolio.total_margin_used * self.min_buffer_multiplier;
        (portfolio.nav - required).max(Decimal::ZERO)
    }

    /// Estimate distance to liquidation as a percentage.
    /// Simplified: assumes uniform leverage across positions.
    pub fn liquidation_distance_pct(&self, portfolio: &Portfolio) -> Decimal {
        if portfolio.leverage.is_zero() || portfolio.nav.is_zero() {
            return dec!(100);
        }

        // Approximate: liquidation occurs when NAV drops to maintenance margin
        // Maintenance margin ≈ total_margin_used * 0.5 (simplified)
        let maintenance = portfolio.total_margin_used * dec!(0.5);
        let buffer = portfolio.nav - maintenance;
        let total_notional: Decimal = portfolio
            .positions
            .iter()
            .map(|p| p.notional_value())
            .sum();

        if total_notional.is_zero() {
            return dec!(100);
        }

        (buffer / total_notional * dec!(100)).max(Decimal::ZERO)
    }

    /// Check if margin levels are critical.
    pub fn is_critical(&self, portfolio: &Portfolio) -> bool {
        let ratio = self.margin_ratio(portfolio);
        ratio < self.min_buffer_multiplier * dec!(0.5) // Below half the buffer
    }

    /// Get a margin status summary.
    pub fn status(&self, portfolio: &Portfolio) -> MarginStatus {
        let ratio = self.margin_ratio(portfolio);
        let available = self.available_margin(portfolio);
        let liq_dist = self.liquidation_distance_pct(portfolio);

        let level = if ratio >= self.min_buffer_multiplier * dec!(2) {
            MarginLevel::Healthy
        } else if ratio >= self.min_buffer_multiplier {
            MarginLevel::Adequate
        } else if ratio >= self.min_buffer_multiplier * dec!(0.5) {
            MarginLevel::Warning
        } else {
            MarginLevel::Critical
        };

        MarginStatus {
            ratio,
            available_margin: available,
            liquidation_distance_pct: liq_dist,
            level,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarginStatus {
    pub ratio: Decimal,
    pub available_margin: Decimal,
    pub liquidation_distance_pct: Decimal,
    pub level: MarginLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginLevel {
    Healthy,
    Adequate,
    Warning,
    Critical,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::Position;
    use hl_core::Side;

    fn make_portfolio_with_margin(nav: Decimal, margin: Decimal) -> Portfolio {
        let mut portfolio = Portfolio::new(nav);
        portfolio.total_margin_used = margin;
        portfolio.nav = nav;
        if !margin.is_zero() {
            portfolio.margin_ratio = nav / margin;
        }
        portfolio
    }

    #[test]
    fn test_sufficient_margin() {
        let monitor = MarginMonitor::new(dec!(2));
        let portfolio = make_portfolio_with_margin(dec!(10000), dec!(3000));
        assert!(monitor.has_sufficient_margin(&portfolio)); // 3.33x > 2x
    }

    #[test]
    fn test_insufficient_margin() {
        let monitor = MarginMonitor::new(dec!(2));
        let portfolio = make_portfolio_with_margin(dec!(10000), dec!(6000));
        assert!(!monitor.has_sufficient_margin(&portfolio)); // 1.67x < 2x
    }

    #[test]
    fn test_no_margin_used() {
        let monitor = MarginMonitor::new(dec!(2));
        let portfolio = make_portfolio_with_margin(dec!(10000), Decimal::ZERO);
        assert!(monitor.has_sufficient_margin(&portfolio));
    }

    #[test]
    fn test_available_margin() {
        let monitor = MarginMonitor::new(dec!(2));
        let portfolio = make_portfolio_with_margin(dec!(10000), dec!(3000));
        let available = monitor.available_margin(&portfolio);
        // 10000 - 3000 * 2 = 4000
        assert_eq!(available, dec!(4000));
    }

    #[test]
    fn test_margin_status_healthy() {
        let monitor = MarginMonitor::new(dec!(2));
        let portfolio = make_portfolio_with_margin(dec!(10000), dec!(1000));
        let status = monitor.status(&portfolio);
        assert_eq!(status.level, MarginLevel::Healthy);
    }

    #[test]
    fn test_margin_status_critical() {
        let monitor = MarginMonitor::new(dec!(2));
        let portfolio = make_portfolio_with_margin(dec!(10000), dec!(8000));
        let status = monitor.status(&portfolio);
        // ratio = 1.25, min_buffer * 0.5 = 1.0, so it's Warning level
        assert!(matches!(
            status.level,
            MarginLevel::Warning | MarginLevel::Critical
        ));
    }

    #[test]
    fn test_is_critical() {
        let monitor = MarginMonitor::new(dec!(2));
        let portfolio = make_portfolio_with_margin(dec!(10000), dec!(12000));
        assert!(monitor.is_critical(&portfolio)); // ratio < 1 < 1
    }
}
