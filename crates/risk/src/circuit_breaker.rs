use rust_decimal::prelude::Signed;
use chrono::{DateTime, Utc};
use hl_core::Portfolio;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{error, info, warn};

/// Circuit breaker status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BreakerStatus {
    Armed,
    Tripped,
    Cooldown,
}

/// Individual circuit breaker.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    pub name: String,
    pub status: BreakerStatus,
    pub tripped_at: Option<DateTime<Utc>>,
    pub cooldown_secs: u64,
    pub trip_count: u64,
    pub reason: String,
}

impl CircuitBreaker {
    pub fn new(name: &str, cooldown_secs: u64) -> Self {
        Self {
            name: name.to_string(),
            status: BreakerStatus::Armed,
            tripped_at: None,
            cooldown_secs,
            trip_count: 0,
            reason: String::new(),
        }
    }

    pub fn trip(&mut self, reason: &str) {
        self.status = BreakerStatus::Tripped;
        self.tripped_at = Some(Utc::now());
        self.trip_count += 1;
        self.reason = reason.to_string();
        error!("Circuit breaker TRIPPED: {} — {}", self.name, reason);
    }

    pub fn check_cooldown(&mut self) -> bool {
        if self.status != BreakerStatus::Tripped {
            return false;
        }
        if let Some(tripped_at) = self.tripped_at {
            let elapsed = (Utc::now() - tripped_at).num_seconds() as u64;
            if elapsed >= self.cooldown_secs {
                self.status = BreakerStatus::Armed;
                self.reason.clear();
                info!("Circuit breaker reset: {}", self.name);
                return true;
            }
        }
        false
    }

    pub fn is_tripped(&self) -> bool {
        self.status == BreakerStatus::Tripped
    }

    pub fn manual_reset(&mut self) {
        self.status = BreakerStatus::Armed;
        self.reason.clear();
        info!("Circuit breaker manually reset: {}", self.name);
    }
}

/// Engine managing multiple independent circuit breakers.
pub struct CircuitBreakerEngine {
    breakers: HashMap<String, CircuitBreaker>,
    /// Global halt flag (manual override).
    manual_halt: bool,
}

impl CircuitBreakerEngine {
    pub fn new() -> Self {
        let mut breakers = HashMap::new();

        // 1. DrawdownBreaker — daily/total loss limits
        breakers.insert(
            "drawdown".to_string(),
            CircuitBreaker::new("DrawdownBreaker", 3600),
        );

        // 2. MarginBreaker — portfolio margin < 2x liquidation
        breakers.insert(
            "margin".to_string(),
            CircuitBreaker::new("MarginBreaker", 1800),
        );

        // 3. VolatilityBreaker — realized vol > 3x historical
        breakers.insert(
            "volatility".to_string(),
            CircuitBreaker::new("VolatilityBreaker", 3600),
        );

        // 4. CorrelationBreaker — correlation spike (regime change)
        breakers.insert(
            "correlation".to_string(),
            CircuitBreaker::new("CorrelationBreaker", 7200),
        );

        // 5. APIBreaker — repeated API errors / latency spike
        breakers.insert(
            "api".to_string(),
            CircuitBreaker::new("APIBreaker", 300),
        );

        // 6. FundingBreaker — unexpected funding rate reversal
        breakers.insert(
            "funding".to_string(),
            CircuitBreaker::new("FundingBreaker", 1800),
        );

        // 7. ManualBreaker — Telegram command to kill all
        breakers.insert(
            "manual".to_string(),
            CircuitBreaker::new("ManualBreaker", 0), // No auto-reset
        );

        Self {
            breakers,
            manual_halt: false,
        }
    }

    /// Check if any breaker is tripped (trading should halt).
    pub fn any_tripped(&self) -> bool {
        self.manual_halt || self.breakers.values().any(|b| b.is_tripped())
    }

    /// Get all tripped breakers.
    pub fn tripped_breakers(&self) -> Vec<&CircuitBreaker> {
        self.breakers.values().filter(|b| b.is_tripped()).collect()
    }

    /// Trip a specific breaker.
    pub fn trip(&mut self, breaker_name: &str, reason: &str) {
        if let Some(breaker) = self.breakers.get_mut(breaker_name) {
            breaker.trip(reason);
        } else {
            warn!("Unknown circuit breaker: {}", breaker_name);
        }
    }

    /// Check and reset expired cooldowns.
    pub fn check_cooldowns(&mut self) {
        for breaker in self.breakers.values_mut() {
            breaker.check_cooldown();
        }
    }

    /// Evaluate portfolio conditions and trip breakers as needed.
    pub fn evaluate(&mut self, portfolio: &Portfolio) {
        // Drawdown check
        let dd = portfolio.drawdown_from_hwm();
        if dd > dec!(20) {
            self.trip("drawdown", &format!("Total drawdown {:.2}% exceeds 20%", dd));
        }
        let daily = portfolio.daily_return_pct();
        if daily < dec!(-10) {
            self.trip("drawdown", &format!("Daily loss {:.2}% exceeds -10%", daily));
        }

        // Margin check
        if !portfolio.total_margin_used.is_zero() {
            let margin_ratio = portfolio.nav / portfolio.total_margin_used;
            if margin_ratio < dec!(2) {
                self.trip(
                    "margin",
                    &format!("Margin ratio {:.2} below 2x buffer", margin_ratio),
                );
            }
        }
    }

    /// Trip the volatility breaker.
    pub fn trip_volatility(&mut self, current_vol: Decimal, historical_vol: Decimal) {
        if !historical_vol.is_zero() && current_vol > historical_vol * dec!(3) {
            self.trip(
                "volatility",
                &format!(
                    "Realized vol {:.4} > 3x historical {:.4}",
                    current_vol, historical_vol
                ),
            );
        }
    }

    /// Trip the correlation breaker on regime change.
    pub fn trip_correlation(&mut self, avg_correlation: Decimal) {
        if avg_correlation > dec!(0.8) {
            self.trip(
                "correlation",
                &format!("Average correlation spike: {:.3}", avg_correlation),
            );
        }
    }

    /// Trip the API breaker on repeated errors.
    pub fn trip_api(&mut self, error_count: u32, window_secs: u64) {
        if error_count > 5 {
            self.trip(
                "api",
                &format!("{} API errors in {}s", error_count, window_secs),
            );
        }
    }

    /// Trip the funding breaker on unexpected reversal.
    pub fn trip_funding(&mut self, asset: &str, old_rate: Decimal, new_rate: Decimal) {
        if old_rate.signum() != new_rate.signum() && old_rate.abs() > dec!(0.001) {
            self.trip(
                "funding",
                &format!(
                    "Funding reversal for {}: {:.6} → {:.6}",
                    asset, old_rate, new_rate
                ),
            );
        }
    }

    /// Manual halt (from Telegram command).
    pub fn manual_halt(&mut self) {
        self.manual_halt = true;
        self.trip("manual", "Manual halt triggered via Telegram");
        warn!("MANUAL HALT ACTIVATED — all trading stopped");
    }

    /// Manual resume.
    pub fn manual_resume(&mut self) {
        self.manual_halt = false;
        if let Some(breaker) = self.breakers.get_mut("manual") {
            breaker.manual_reset();
        }
        info!("Manual halt lifted — trading resumed");
    }

    /// Reset all breakers (emergency).
    pub fn reset_all(&mut self) {
        self.manual_halt = false;
        for breaker in self.breakers.values_mut() {
            breaker.manual_reset();
        }
        info!("All circuit breakers reset");
    }

    /// Get a status summary.
    pub fn status_summary(&self) -> String {
        let mut summary = String::from("Circuit Breakers:\n");
        for (name, breaker) in &self.breakers {
            let status = match breaker.status {
                BreakerStatus::Armed => "🟢 Armed",
                BreakerStatus::Tripped => "🔴 TRIPPED",
                BreakerStatus::Cooldown => "🟡 Cooldown",
            };
            summary.push_str(&format!("  {}: {} (trips: {})\n", name, status, breaker.trip_count));
            if breaker.is_tripped() {
                summary.push_str(&format!("    Reason: {}\n", breaker.reason));
            }
        }
        if self.manual_halt {
            summary.push_str("  ⛔ MANUAL HALT ACTIVE\n");
        }
        summary
    }

    /// Get total number of breakers.
    pub fn breaker_count(&self) -> usize {
        self.breakers.len()
    }
}

impl Default for CircuitBreakerEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_creation() {
        let engine = CircuitBreakerEngine::new();
        assert_eq!(engine.breaker_count(), 7);
        assert!(!engine.any_tripped());
    }

    #[test]
    fn test_trip_breaker() {
        let mut engine = CircuitBreakerEngine::new();
        engine.trip("drawdown", "Test trip");
        assert!(engine.any_tripped());
        assert_eq!(engine.tripped_breakers().len(), 1);
    }

    #[test]
    fn test_manual_halt() {
        let mut engine = CircuitBreakerEngine::new();
        engine.manual_halt();
        assert!(engine.any_tripped());

        engine.manual_resume();
        assert!(!engine.any_tripped());
    }

    #[test]
    fn test_reset_all() {
        let mut engine = CircuitBreakerEngine::new();
        engine.trip("drawdown", "Test");
        engine.trip("margin", "Test");
        engine.manual_halt();

        engine.reset_all();
        assert!(!engine.any_tripped());
    }

    #[test]
    fn test_evaluate_drawdown() {
        let mut engine = CircuitBreakerEngine::new();
        let mut portfolio = Portfolio::new(dec!(10000));
        portfolio.high_water_mark = dec!(13000);
        portfolio.nav = dec!(10000); // ~23% drawdown

        engine.evaluate(&portfolio);
        assert!(engine.any_tripped());
    }

    #[test]
    fn test_evaluate_daily_loss() {
        let mut engine = CircuitBreakerEngine::new();
        let mut portfolio = Portfolio::new(dec!(10000));
        portfolio.daily_starting_nav = dec!(12000);
        portfolio.nav = dec!(10000); // -16.67% daily

        engine.evaluate(&portfolio);
        assert!(engine.any_tripped());
    }

    #[test]
    fn test_trip_volatility() {
        let mut engine = CircuitBreakerEngine::new();
        engine.trip_volatility(dec!(0.06), dec!(0.02)); // 3x historical
        // 0.06 > 0.02 * 3 = 0.06, should be equal not greater
        // 0.061 would trip it
        assert!(!engine.any_tripped());

        engine.trip_volatility(dec!(0.07), dec!(0.02)); // > 3x
        assert!(engine.any_tripped());
    }

    #[test]
    fn test_trip_correlation() {
        let mut engine = CircuitBreakerEngine::new();
        engine.trip_correlation(dec!(0.85));
        assert!(engine.any_tripped());
    }

    #[test]
    fn test_trip_api() {
        let mut engine = CircuitBreakerEngine::new();
        engine.trip_api(3, 60);
        assert!(!engine.any_tripped()); // 3 < 5

        engine.trip_api(6, 60);
        assert!(engine.any_tripped());
    }

    #[test]
    fn test_trip_funding_reversal() {
        let mut engine = CircuitBreakerEngine::new();
        engine.trip_funding("BTC", dec!(0.002), dec!(-0.001));
        assert!(engine.any_tripped());
    }

    #[test]
    fn test_status_summary() {
        let engine = CircuitBreakerEngine::new();
        let summary = engine.status_summary();
        assert!(summary.contains("Circuit Breakers"));
        assert!(summary.contains("Armed"));
    }

    #[test]
    fn test_breaker_cooldown() {
        let mut breaker = CircuitBreaker::new("test", 0); // Instant cooldown
        breaker.trip("test reason");
        assert!(breaker.is_tripped());

        // With 0 cooldown, should reset immediately
        assert!(breaker.check_cooldown());
        assert!(!breaker.is_tripped());
    }
}
