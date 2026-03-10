use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hl_core::{Fill, MarketContext, Side, Signal, StrategyRiskParams, StrategyType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::VecDeque;
use tracing::{debug, info, warn};

/// Maximum NAV percentage per LLM signal.
const MAX_NAV_PER_SIGNAL: Decimal = dec!(5);
/// Maximum number of active LLM-driven positions.
const MAX_LLM_POSITIONS: usize = 5;
/// Minimum confidence threshold to accept an LLM signal.
const MIN_LLM_CONFIDENCE: Decimal = dec!(0.6);
/// Maximum signal age before discard (in seconds).
const MAX_SIGNAL_AGE_SECS: i64 = 300;
/// Calibration window for tracking accuracy.
const CALIBRATION_WINDOW: usize = 100;

/// Incoming LLM signal from gRPC.
#[derive(Debug, Clone)]
pub struct LlmIncomingSignal {
    pub asset: String,
    pub direction: String,
    pub confidence: f64,
    pub edge: f64,
    pub suggested_leverage: f64,
    pub signal_source: String,
    pub reasoning: String,
    pub received_at: DateTime<Utc>,
}

/// Tracking record for calibration.
#[derive(Debug, Clone)]
struct CalibrationRecord {
    predicted_direction: Side,
    confidence: Decimal,
    actual_pnl: Decimal,
    was_correct: bool,
    timestamp: DateTime<Utc>,
}

/// LLM-Driven Signal Strategy.
///
/// Receives trading signals from external LLM systems via gRPC.
/// Applies position sizing via Kelly criterion with a mandatory
/// risk overlay. Tracks accuracy over time for calibration.
pub struct LlmSignalStrategy {
    name: String,
    active_signals: Vec<Signal>,
    /// Queue of pending signals from gRPC.
    pending_signals: VecDeque<LlmIncomingSignal>,
    /// Track accuracy for calibration.
    calibration: VecDeque<CalibrationRecord>,
    /// Number of active positions from LLM signals.
    active_position_count: usize,
    /// Running accuracy stats.
    total_signals: u64,
    correct_signals: u64,
}

impl LlmSignalStrategy {
    pub fn new() -> Self {
        Self {
            name: "LlmSignal".to_string(),
            active_signals: Vec::new(),
            pending_signals: VecDeque::new(),
            calibration: VecDeque::new(),
            active_position_count: 0,
            total_signals: 0,
            correct_signals: 0,
        }
    }

    /// Push an incoming signal from gRPC into the processing queue.
    pub fn push_signal(&mut self, signal: LlmIncomingSignal) {
        info!(
            "LLM signal received: {} {} confidence={:.2}% source={}",
            signal.direction, signal.asset, signal.confidence * 100.0, signal.signal_source
        );
        self.pending_signals.push_back(signal);
    }

    /// Get current accuracy rate.
    pub fn accuracy_rate(&self) -> Decimal {
        if self.total_signals == 0 {
            return dec!(0.5); // Prior: 50%
        }
        Decimal::from(self.correct_signals) / Decimal::from(self.total_signals)
    }

    /// Get calibration stats.
    pub fn calibration_stats(&self) -> CalibrationStats {
        let total = self.calibration.len();
        let correct = self.calibration.iter().filter(|r| r.was_correct).count();
        let total_pnl: Decimal = self.calibration.iter().map(|r| r.actual_pnl).sum();

        // Brier score: average (forecast - outcome)²
        let brier: f64 = if total > 0 {
            self.calibration
                .iter()
                .map(|r| {
                    let outcome = if r.was_correct { 1.0 } else { 0.0 };
                    let conf: f64 = r.confidence.to_string().parse().unwrap_or(0.5);
                    (conf - outcome).powi(2)
                })
                .sum::<f64>()
                / total as f64
        } else {
            0.0
        };

        CalibrationStats {
            total_signals: total,
            correct,
            accuracy: if total > 0 {
                correct as f64 / total as f64
            } else {
                0.0
            },
            total_pnl,
            brier_score: brier,
        }
    }

    /// Record outcome for calibration tracking.
    pub fn record_outcome(&mut self, direction: Side, confidence: Decimal, pnl: Decimal) {
        let was_correct = pnl > Decimal::ZERO;
        self.calibration.push_back(CalibrationRecord {
            predicted_direction: direction,
            confidence,
            actual_pnl: pnl,
            was_correct,
            timestamp: Utc::now(),
        });

        if self.calibration.len() > CALIBRATION_WINDOW {
            self.calibration.pop_front();
        }

        self.total_signals += 1;
        if was_correct {
            self.correct_signals += 1;
        }
    }

    /// Validate and convert an incoming signal.
    fn validate_signal(&self, incoming: &LlmIncomingSignal) -> Option<Signal> {
        // Check age
        let age = (Utc::now() - incoming.received_at).num_seconds();
        if age > MAX_SIGNAL_AGE_SECS {
            warn!(
                "LLM signal too old ({age}s): {} {}",
                incoming.direction, incoming.asset
            );
            return None;
        }

        // Check confidence
        let confidence = Decimal::from_f64_retain(incoming.confidence)?;
        if confidence < MIN_LLM_CONFIDENCE {
            warn!(
                "LLM signal confidence too low ({:.2}%): {} {}",
                incoming.confidence * 100.0,
                incoming.direction,
                incoming.asset
            );
            return None;
        }

        // Parse direction
        let side = match incoming.direction.to_lowercase().as_str() {
            "long" | "buy" => Side::Long,
            "short" | "sell" => Side::Short,
            _ => {
                warn!("Invalid LLM signal direction: {}", incoming.direction);
                return None;
            }
        };

        let edge = Decimal::from_f64_retain(incoming.edge).unwrap_or(dec!(0.01));
        let leverage = Decimal::from_f64_retain(incoming.suggested_leverage)
            .unwrap_or(dec!(1))
            .min(dec!(5)); // Cap leverage

        let signal = Signal::new(
            &incoming.asset,
            side,
            confidence,
            edge,
            StrategyType::LlmSignal,
            &format!(
                "LLM signal from {}: {}",
                incoming.signal_source, incoming.reasoning
            ),
        )
        .with_leverage(leverage);

        Some(signal)
    }
}

impl Default for LlmSignalStrategy {
    fn default() -> Self {
        Self::new()
    }
}

/// Calibration statistics.
#[derive(Debug, Clone)]
pub struct CalibrationStats {
    pub total_signals: usize,
    pub correct: usize,
    pub accuracy: f64,
    pub total_pnl: Decimal,
    pub brier_score: f64,
}

#[async_trait]
impl crate::Strategy for LlmSignalStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn strategy_type(&self) -> StrategyType {
        StrategyType::LlmSignal
    }

    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();
        self.active_signals.clear();

        // Process pending signals from gRPC
        while let Some(incoming) = self.pending_signals.pop_front() {
            if self.active_position_count >= MAX_LLM_POSITIONS {
                warn!("LLM: Max positions reached, dropping signal for {}", incoming.asset);
                continue;
            }

            if let Some(mut signal) = self.validate_signal(&incoming) {
                // Apply NAV cap
                let max_size = ctx.portfolio.nav * MAX_NAV_PER_SIGNAL / dec!(100);
                let price = ctx
                    .prices
                    .get(&incoming.asset)
                    .copied()
                    .unwrap_or(dec!(1));
                if !price.is_zero() {
                    let max_units = max_size / price;
                    signal = signal.with_size(max_units);
                }

                info!(
                    "LLM signal accepted: {} {} confidence={:.2}",
                    signal.side, signal.asset, signal.confidence
                );
                self.active_signals.push(signal.clone());
                signals.push(signal);
            }
        }

        Ok(signals)
    }

    async fn on_fill(&mut self, fill: &Fill) -> Result<()> {
        self.active_position_count += 1;
        info!(
            "LLM fill: {} {} @ {} | active_positions={}",
            fill.side, fill.asset, fill.price, self.active_position_count
        );
        Ok(())
    }

    fn active_signals(&self) -> &[Signal] {
        &self.active_signals
    }

    fn risk_params(&self) -> StrategyRiskParams {
        StrategyRiskParams {
            max_position_pct: MAX_NAV_PER_SIGNAL,
            max_leverage: dec!(3),
            max_drawdown_pct: dec!(5),
            max_correlated_exposure: dec!(15),
            cooldown_secs: 60,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::Portfolio;
    use std::collections::HashMap;

    fn make_incoming(asset: &str, direction: &str, confidence: f64) -> LlmIncomingSignal {
        LlmIncomingSignal {
            asset: asset.to_string(),
            direction: direction.to_string(),
            confidence,
            edge: 0.05,
            suggested_leverage: 2.0,
            signal_source: "test".to_string(),
            reasoning: "Test signal".to_string(),
            received_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_llm_signal_acceptance() {
        let mut strategy = LlmSignalStrategy::new();
        strategy.push_signal(make_incoming("BTC", "long", 0.8));

        let mut prices = HashMap::new();
        prices.insert("BTC".to_string(), dec!(50000));

        let ctx = MarketContext {
            assets: Vec::new(),
            prices,
            funding_rates: HashMap::new(),
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(10000)),
            timestamp: Utc::now(),
        };

        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].side, Side::Long);
        assert_eq!(signals[0].asset, "BTC");
    }

    #[tokio::test]
    async fn test_llm_signal_low_confidence_rejection() {
        let mut strategy = LlmSignalStrategy::new();
        strategy.push_signal(make_incoming("BTC", "long", 0.3)); // Too low

        let ctx = MarketContext {
            assets: Vec::new(),
            prices: HashMap::new(),
            funding_rates: HashMap::new(),
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(10000)),
            timestamp: Utc::now(),
        };

        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert!(signals.is_empty());
    }

    #[tokio::test]
    async fn test_llm_signal_max_positions() {
        let mut strategy = LlmSignalStrategy::new();
        strategy.active_position_count = MAX_LLM_POSITIONS;
        strategy.push_signal(make_incoming("BTC", "long", 0.9));

        let ctx = MarketContext {
            assets: Vec::new(),
            prices: HashMap::new(),
            funding_rates: HashMap::new(),
            orderbooks: HashMap::new(),
            portfolio: Portfolio::new(dec!(10000)),
            timestamp: Utc::now(),
        };

        let signals = strategy.evaluate(&ctx).await.unwrap();
        assert!(signals.is_empty());
    }

    #[test]
    fn test_calibration_tracking() {
        let mut strategy = LlmSignalStrategy::new();
        strategy.record_outcome(Side::Long, dec!(0.8), dec!(100));
        strategy.record_outcome(Side::Long, dec!(0.7), dec!(-50));
        strategy.record_outcome(Side::Short, dec!(0.9), dec!(200));

        let stats = strategy.calibration_stats();
        assert_eq!(stats.total_signals, 3);
        assert_eq!(stats.correct, 2);
        assert!(stats.accuracy > 0.6);
        assert_eq!(stats.total_pnl, dec!(250));
    }

    #[test]
    fn test_accuracy_rate() {
        let mut strategy = LlmSignalStrategy::new();
        assert_eq!(strategy.accuracy_rate(), dec!(0.5)); // Prior

        strategy.total_signals = 10;
        strategy.correct_signals = 7;
        assert_eq!(strategy.accuracy_rate(), dec!(0.7));
    }

    #[test]
    fn test_validate_invalid_direction() {
        let strategy = LlmSignalStrategy::new();
        let incoming = make_incoming("BTC", "invalid", 0.9);
        assert!(strategy.validate_signal(&incoming).is_none());
    }

    #[test]
    fn test_risk_params() {
        let strategy = LlmSignalStrategy::new();
        let params = strategy.risk_params();
        assert_eq!(params.max_position_pct, MAX_NAV_PER_SIGNAL);
    }
}
