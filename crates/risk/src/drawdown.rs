use chrono::{DateTime, Utc};
use hl_core::Portfolio;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::{info, warn};

/// Drawdown monitoring with progressive position reduction and kill switches.
pub struct DrawdownMonitor {
    pub daily_limit: Decimal,
    pub total_limit: Decimal,
    /// Progressive reduction starts at this drawdown %.
    pub reduction_start: Decimal,
    /// Cooldown period after a breach (seconds).
    pub cooldown_secs: u64,
    /// Last breach timestamp.
    last_breach: Option<DateTime<Utc>>,
    /// Current state.
    state: DrawdownState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawdownState {
    Normal,
    Reducing,
    Halted,
    Cooldown,
}

impl DrawdownMonitor {
    pub fn new(daily_limit: Decimal, total_limit: Decimal) -> Self {
        Self {
            daily_limit,
            total_limit,
            reduction_start: dec!(5), // Start reducing at 5% drawdown
            cooldown_secs: 3600,      // 1 hour cooldown
            last_breach: None,
            state: DrawdownState::Normal,
        }
    }

    /// Update the drawdown state based on current portfolio.
    pub fn update(&mut self, portfolio: &Portfolio) -> DrawdownState {
        // Check if we're in cooldown
        if let Some(breach_time) = self.last_breach {
            let elapsed = (Utc::now() - breach_time).num_seconds() as u64;
            if elapsed < self.cooldown_secs {
                self.state = DrawdownState::Cooldown;
                return self.state;
            }
        }

        let total_dd = portfolio.drawdown_from_hwm();
        let daily_dd = portfolio.daily_return_pct();

        if total_dd >= self.total_limit || daily_dd <= -self.daily_limit {
            self.state = DrawdownState::Halted;
            self.last_breach = Some(Utc::now());
            warn!(
                "DRAWDOWN BREACH: total_dd={:.2}%, daily_dd={:.2}%",
                total_dd, daily_dd
            );
        } else if total_dd >= self.reduction_start {
            self.state = DrawdownState::Reducing;
            info!("Drawdown reduction active: {:.2}%", total_dd);
        } else {
            self.state = DrawdownState::Normal;
        }

        self.state
    }

    /// Compute reduction factor (1.0 = no reduction, 0.0 = close all).
    /// Progressive: linearly reduce from 1.0 at reduction_start to 0.0 at total_limit.
    pub fn reduction_factor(&self, portfolio: &Portfolio) -> Decimal {
        let dd = portfolio.drawdown_from_hwm();

        if dd <= self.reduction_start {
            return Decimal::ONE;
        }

        if dd >= self.total_limit {
            return Decimal::ZERO;
        }

        let range = self.total_limit - self.reduction_start;
        if range.is_zero() {
            return Decimal::ZERO;
        }

        Decimal::ONE - (dd - self.reduction_start) / range
    }

    /// Check if trading should be halted.
    pub fn is_halted(&self) -> bool {
        matches!(self.state, DrawdownState::Halted | DrawdownState::Cooldown)
    }

    /// Check if position reduction is active.
    pub fn is_reducing(&self) -> bool {
        matches!(self.state, DrawdownState::Reducing)
    }

    /// Get the current state.
    pub fn state(&self) -> DrawdownState {
        self.state
    }

    /// Manually reset after a breach (e.g., after review).
    pub fn reset(&mut self) {
        self.state = DrawdownState::Normal;
        self.last_breach = None;
        info!("Drawdown monitor reset");
    }

    /// Get time remaining in cooldown (seconds), or 0 if not in cooldown.
    pub fn cooldown_remaining(&self) -> u64 {
        if let Some(breach_time) = self.last_breach {
            let elapsed = (Utc::now() - breach_time).num_seconds() as u64;
            if elapsed < self.cooldown_secs {
                return self.cooldown_secs - elapsed;
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_portfolio(nav: Decimal, hwm: Decimal, daily_start: Decimal) -> Portfolio {
        let mut p = Portfolio::new(nav);
        p.nav = nav;
        p.high_water_mark = hwm;
        p.daily_starting_nav = daily_start;
        p
    }

    #[test]
    fn test_normal_state() {
        let mut monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        let portfolio = make_portfolio(dec!(10000), dec!(10200), dec!(10000));
        let state = monitor.update(&portfolio);
        assert_eq!(state, DrawdownState::Normal);
    }

    #[test]
    fn test_reducing_state() {
        let mut monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        // 8% drawdown from HWM → should be Reducing (> 5%)
        let portfolio = make_portfolio(dec!(9200), dec!(10000), dec!(9500));
        let state = monitor.update(&portfolio);
        assert_eq!(state, DrawdownState::Reducing);
    }

    #[test]
    fn test_halted_total_dd() {
        let mut monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        // 25% drawdown from HWM → halted
        let portfolio = make_portfolio(dec!(7500), dec!(10000), dec!(8000));
        let state = monitor.update(&portfolio);
        assert_eq!(state, DrawdownState::Halted);
    }

    #[test]
    fn test_halted_daily_dd() {
        let mut monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        // -12% daily return → halted
        let portfolio = make_portfolio(dec!(8800), dec!(10000), dec!(10000));
        let state = monitor.update(&portfolio);
        assert_eq!(state, DrawdownState::Halted);
    }

    #[test]
    fn test_reduction_factor_normal() {
        let monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        let portfolio = make_portfolio(dec!(9800), dec!(10000), dec!(10000));
        assert_eq!(monitor.reduction_factor(&portfolio), Decimal::ONE);
    }

    #[test]
    fn test_reduction_factor_midway() {
        let monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        // 12.5% drawdown: halfway between 5% and 20%
        let portfolio = make_portfolio(dec!(8750), dec!(10000), dec!(10000));
        let factor = monitor.reduction_factor(&portfolio);
        assert!(factor > Decimal::ZERO);
        assert!(factor < Decimal::ONE);
        // (12.5 - 5) / (20 - 5) = 7.5/15 = 0.5, factor = 1 - 0.5 = 0.5
        assert_eq!(factor, dec!(0.5));
    }

    #[test]
    fn test_reduction_factor_at_limit() {
        let monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        let portfolio = make_portfolio(dec!(8000), dec!(10000), dec!(10000));
        assert_eq!(monitor.reduction_factor(&portfolio), Decimal::ZERO);
    }

    #[test]
    fn test_reset() {
        let mut monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        let portfolio = make_portfolio(dec!(7500), dec!(10000), dec!(8000));
        monitor.update(&portfolio);
        assert!(monitor.is_halted());

        monitor.reset();
        assert_eq!(monitor.state(), DrawdownState::Normal);
        assert!(!monitor.is_halted());
    }

    #[test]
    fn test_is_reducing() {
        let mut monitor = DrawdownMonitor::new(dec!(10), dec!(20));
        let portfolio = make_portfolio(dec!(9200), dec!(10000), dec!(9500));
        monitor.update(&portfolio);
        assert!(monitor.is_reducing());
    }
}
