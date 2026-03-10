use anyhow::Result;
use hl_core::{Portfolio, Signal, StrategyRiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

use crate::drawdown::DrawdownMonitor;
use crate::kelly::KellySizer;
use crate::margin::MarginMonitor;

/// Rejection reason for a signal.
#[derive(Debug, Clone)]
pub enum RejectionReason {
    MaxPortfolioLeverage { current: Decimal, max: Decimal },
    MaxPositionSize { size_pct: Decimal, max_pct: Decimal },
    MaxCorrelatedExposure { exposure: Decimal, max: Decimal },
    DrawdownBreach { drawdown: Decimal, limit: Decimal },
    MarginInsufficient { ratio: Decimal, required: Decimal },
    CircuitBreakerActive { breaker: String },
    InsufficientEdge { edge: Decimal, min: Decimal },
    CooldownActive { remaining_secs: u64 },
}

impl std::fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RejectionReason::MaxPortfolioLeverage { current, max } => {
                write!(f, "Portfolio leverage {current}x exceeds max {max}x")
            }
            RejectionReason::MaxPositionSize { size_pct, max_pct } => {
                write!(f, "Position {size_pct}% exceeds max {max_pct}%")
            }
            RejectionReason::MaxCorrelatedExposure { exposure, max } => {
                write!(f, "Correlated exposure {exposure}% exceeds max {max}%")
            }
            RejectionReason::DrawdownBreach { drawdown, limit } => {
                write!(f, "Drawdown {drawdown}% exceeds limit {limit}%")
            }
            RejectionReason::MarginInsufficient { ratio, required } => {
                write!(f, "Margin ratio {ratio} below required {required}")
            }
            RejectionReason::CircuitBreakerActive { breaker } => {
                write!(f, "Circuit breaker active: {breaker}")
            }
            RejectionReason::InsufficientEdge { edge, min } => {
                write!(f, "Edge {edge} below minimum {min}")
            }
            RejectionReason::CooldownActive { remaining_secs } => {
                write!(f, "Strategy cooldown: {remaining_secs}s remaining")
            }
        }
    }
}

/// Portfolio-level risk manager that aggregates risk across all strategies.
pub struct PortfolioRiskManager {
    pub max_portfolio_leverage: Decimal,
    pub max_position_pct: Decimal,
    pub max_correlated_pct: Decimal,
    pub drawdown_monitor: DrawdownMonitor,
    pub margin_monitor: MarginMonitor,
    pub kelly_sizer: KellySizer,
    pub min_edge: Decimal,
}

impl PortfolioRiskManager {
    pub fn new(
        max_portfolio_leverage: Decimal,
        max_position_pct: Decimal,
        max_correlated_pct: Decimal,
        daily_drawdown_limit: Decimal,
        total_drawdown_limit: Decimal,
    ) -> Self {
        Self {
            max_portfolio_leverage,
            max_position_pct,
            max_correlated_pct,
            drawdown_monitor: DrawdownMonitor::new(daily_drawdown_limit, total_drawdown_limit),
            margin_monitor: MarginMonitor::new(dec!(2)),
            kelly_sizer: KellySizer::new(dec!(0.25), dec!(5)),
            min_edge: dec!(0.005), // 0.5% minimum edge
        }
    }

    pub fn from_config(config: &hl_core::config::RiskConfig) -> Self {
        Self::new(
            config.max_portfolio_leverage,
            config.max_position_pct,
            config.max_correlated_pct,
            config.daily_drawdown_limit,
            config.total_drawdown_limit,
        )
    }

    /// Evaluate whether a signal should be accepted or rejected.
    pub fn evaluate_signal(
        &self,
        signal: &Signal,
        portfolio: &Portfolio,
        strategy_params: &StrategyRiskParams,
    ) -> Result<Decimal, RejectionReason> {
        // 1. Check portfolio leverage
        if portfolio.leverage >= self.max_portfolio_leverage {
            return Err(RejectionReason::MaxPortfolioLeverage {
                current: portfolio.leverage,
                max: self.max_portfolio_leverage,
            });
        }

        // 2. Check drawdown
        let daily_dd = portfolio.drawdown_from_hwm();
        if daily_dd > self.drawdown_monitor.total_limit {
            return Err(RejectionReason::DrawdownBreach {
                drawdown: daily_dd,
                limit: self.drawdown_monitor.total_limit,
            });
        }

        let daily_return = portfolio.daily_return_pct();
        if daily_return < -self.drawdown_monitor.daily_limit {
            return Err(RejectionReason::DrawdownBreach {
                drawdown: daily_return.abs(),
                limit: self.drawdown_monitor.daily_limit,
            });
        }

        // 3. Check margin
        if !self.margin_monitor.has_sufficient_margin(portfolio) {
            return Err(RejectionReason::MarginInsufficient {
                ratio: portfolio.margin_ratio,
                required: self.margin_monitor.min_buffer_multiplier,
            });
        }

        // 4. Check minimum edge
        if signal.edge < self.min_edge {
            return Err(RejectionReason::InsufficientEdge {
                edge: signal.edge,
                min: self.min_edge,
            });
        }

        // 5. Compute position size via Kelly
        let kelly_size = self.kelly_sizer.compute_size(
            signal.confidence,
            signal.edge,
            signal.suggested_leverage,
        );

        // 6. Cap at max position size
        let max_size = portfolio.nav * self.max_position_pct / dec!(100);
        let position_size = kelly_size.min(max_size);

        // 7. Cap at strategy-level limits
        let strategy_max = portfolio.nav * strategy_params.max_position_pct / dec!(100);
        let final_size = position_size.min(strategy_max);

        if final_size <= Decimal::ZERO {
            return Err(RejectionReason::MaxPositionSize {
                size_pct: Decimal::ZERO,
                max_pct: self.max_position_pct,
            });
        }

        info!(
            "Risk approved: {} {} — kelly={:.4}, capped={:.4}",
            signal.side, signal.asset, kelly_size, final_size
        );

        Ok(final_size)
    }

    /// Determine if positions should be reduced due to drawdown.
    pub fn drawdown_reduction_factor(&self, portfolio: &Portfolio) -> Decimal {
        self.drawdown_monitor.reduction_factor(portfolio)
    }

    /// Check if all trading should stop.
    pub fn should_halt(&self, portfolio: &Portfolio) -> bool {
        let dd = portfolio.drawdown_from_hwm();
        dd >= self.drawdown_monitor.total_limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::{Side, StrategyType};

    fn make_risk_manager() -> PortfolioRiskManager {
        PortfolioRiskManager::new(
            dec!(3),   // 3x max leverage
            dec!(15),  // 15% max position
            dec!(30),  // 30% max correlated
            dec!(10),  // 10% daily drawdown
            dec!(20),  // 20% total drawdown
        )
    }

    fn make_signal(asset: &str, confidence: Decimal, edge: Decimal) -> Signal {
        Signal::new(
            asset,
            Side::Long,
            confidence,
            edge,
            StrategyType::FundingArb,
            "test",
        )
        .with_leverage(dec!(2))
    }

    #[test]
    fn test_approve_valid_signal() {
        let rm = make_risk_manager();
        let portfolio = Portfolio::new(dec!(10000));
        let signal = make_signal("BTC", dec!(0.8), dec!(0.05));
        let params = StrategyRiskParams::default();

        let result = rm.evaluate_signal(&signal, &portfolio, &params);
        assert!(result.is_ok());
        let size = result.unwrap();
        assert!(size > Decimal::ZERO);
        assert!(size <= dec!(1500)); // 15% of 10000
    }

    #[test]
    fn test_reject_leverage_exceeded() {
        let rm = make_risk_manager();
        let mut portfolio = Portfolio::new(dec!(10000));
        portfolio.leverage = dec!(3.5); // Above max

        let signal = make_signal("BTC", dec!(0.8), dec!(0.05));
        let params = StrategyRiskParams::default();

        let result = rm.evaluate_signal(&signal, &portfolio, &params);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RejectionReason::MaxPortfolioLeverage { .. }
        ));
    }

    #[test]
    fn test_reject_insufficient_edge() {
        let rm = make_risk_manager();
        let portfolio = Portfolio::new(dec!(10000));
        let signal = make_signal("BTC", dec!(0.8), dec!(0.001)); // Too small
        let params = StrategyRiskParams::default();

        let result = rm.evaluate_signal(&signal, &portfolio, &params);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            RejectionReason::InsufficientEdge { .. }
        ));
    }

    #[test]
    fn test_drawdown_halt() {
        let rm = make_risk_manager();
        let mut portfolio = Portfolio::new(dec!(10000));
        portfolio.high_water_mark = dec!(12000);
        portfolio.nav = dec!(9000); // 25% drawdown

        assert!(rm.should_halt(&portfolio));
    }

    #[test]
    fn test_no_halt_small_drawdown() {
        let rm = make_risk_manager();
        let mut portfolio = Portfolio::new(dec!(10000));
        portfolio.high_water_mark = dec!(10500);
        portfolio.nav = dec!(10000); // ~4.76% drawdown

        assert!(!rm.should_halt(&portfolio));
    }

    #[test]
    fn test_drawdown_reduction_factor() {
        let rm = make_risk_manager();
        let mut portfolio = Portfolio::new(dec!(10000));
        portfolio.high_water_mark = dec!(10800);
        portfolio.nav = dec!(10000); // ~7.4% drawdown

        let factor = rm.drawdown_reduction_factor(&portfolio);
        assert!(factor < Decimal::ONE); // Should be reducing
    }
}
