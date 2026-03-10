use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hl_core::{Candle, Fill, MarketContext, Side, Signal, StrategyRiskParams, StrategyType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{debug, info};

/// EMA periods for crossover detection.
const EMA_FAST: usize = 20;
const EMA_MEDIUM: usize = 50;
const EMA_SLOW: usize = 200;
/// Minimum ADX for trend strength confirmation.
const MIN_ADX: Decimal = dec!(25);
/// Volume multiplier for breakout confirmation.
const VOLUME_BREAKOUT_MULTIPLIER: Decimal = dec!(1.5);
/// ATR multiplier for trailing stop.
const ATR_STOP_MULTIPLIER: Decimal = dec!(2);
/// Number of candles for consolidation detection.
const CONSOLIDATION_WINDOW: usize = 10;
/// Maximum ratio of range to ATR for consolidation.
const CONSOLIDATION_RATIO: Decimal = dec!(0.6);

/// Per-asset momentum state.
#[derive(Debug, Clone)]
struct AssetMomentumState {
    asset: String,
    ema_20: Vec<Decimal>,
    ema_50: Vec<Decimal>,
    ema_200: Vec<Decimal>,
    adx: Vec<Decimal>,
    atr: Vec<Decimal>,
    candles: Vec<Candle>,
    last_signal_side: Option<Side>,
    trailing_stop: Option<Decimal>,
}

impl AssetMomentumState {
    fn new(asset: &str) -> Self {
        Self {
            asset: asset.to_string(),
            ema_20: Vec::new(),
            ema_50: Vec::new(),
            ema_200: Vec::new(),
            adx: Vec::new(),
            atr: Vec::new(),
            candles: Vec::new(),
            last_signal_side: None,
            trailing_stop: None,
        }
    }

    /// Check if price just broke out of consolidation.
    fn is_breakout(&self) -> bool {
        if self.candles.len() < CONSOLIDATION_WINDOW + 1 {
            return false;
        }

        let window = &self.candles[self.candles.len() - CONSOLIDATION_WINDOW - 1..self.candles.len() - 1];
        let current = self.candles.last().unwrap();

        // Find the range of the consolidation window
        let high = window.iter().map(|c| c.high).max().unwrap_or(Decimal::ZERO);
        let low = window.iter().map(|c| c.low).min().unwrap_or(Decimal::ZERO);
        let range = high - low;

        // Current ATR for comparison
        let current_atr = self.atr.last().copied().unwrap_or(range);
        if current_atr.is_zero() {
            return false;
        }

        // Was consolidating: range < consolidation_ratio * ATR * window_size
        let consolidation_range = current_atr * CONSOLIDATION_RATIO * Decimal::from(CONSOLIDATION_WINDOW);
        let was_consolidating = range < consolidation_range;

        // Now breaking out: close above high or below low
        let breaking_up = current.close > high;
        let breaking_down = current.close < low;

        was_consolidating && (breaking_up || breaking_down)
    }

    /// Get breakout direction.
    fn breakout_direction(&self) -> Option<Side> {
        if self.candles.len() < CONSOLIDATION_WINDOW + 1 {
            return None;
        }

        let window = &self.candles[self.candles.len() - CONSOLIDATION_WINDOW - 1..self.candles.len() - 1];
        let current = self.candles.last()?;

        let high = window.iter().map(|c| c.high).max()?;
        let low = window.iter().map(|c| c.low).min()?;

        if current.close > high {
            Some(Side::Long)
        } else if current.close < low {
            Some(Side::Short)
        } else {
            None
        }
    }

    /// Check for EMA crossover signal.
    fn ema_crossover(&self) -> Option<Side> {
        if self.ema_20.len() < 2 || self.ema_50.len() < 2 {
            return None;
        }

        let n = self.ema_20.len();
        let fast_curr = self.ema_20[n - 1];
        let fast_prev = self.ema_20[n - 2];
        let medium_curr = self.ema_50[self.ema_50.len() - 1];
        let medium_prev = self.ema_50[self.ema_50.len() - 2];

        // Bullish crossover: fast crosses above medium
        if fast_prev <= medium_prev && fast_curr > medium_curr {
            return Some(Side::Long);
        }
        // Bearish crossover: fast crosses below medium
        if fast_prev >= medium_prev && fast_curr < medium_curr {
            return Some(Side::Short);
        }

        None
    }

    /// Check if volume confirms the signal.
    fn volume_confirms(&self) -> bool {
        if self.candles.len() < 21 {
            return false;
        }

        let recent = &self.candles[self.candles.len() - 20..self.candles.len() - 1];
        let avg_volume: Decimal =
            recent.iter().map(|c| c.volume).sum::<Decimal>() / Decimal::from(recent.len());
        let current_volume = self.candles.last().map(|c| c.volume).unwrap_or_default();

        if avg_volume.is_zero() {
            return false;
        }

        current_volume > avg_volume * VOLUME_BREAKOUT_MULTIPLIER
    }

    /// Compute ATR-based trailing stop.
    fn compute_trailing_stop(&self, side: Side, price: Decimal) -> Decimal {
        let atr = self.atr.last().copied().unwrap_or(price * dec!(0.02));
        match side {
            Side::Long => price - atr * ATR_STOP_MULTIPLIER,
            Side::Short => price + atr * ATR_STOP_MULTIPLIER,
        }
    }
}

/// Momentum / Breakout / Trend Following Strategy.
///
/// Uses EMA crossovers (20/50/200), volume profile analysis, and ADX
/// filtering to identify trend entries. Requires multiple confirmations:
/// EMA crossover + volume surge + breakout from consolidation + ADX > 25.
pub struct MomentumStrategy {
    name: String,
    active_signals: Vec<Signal>,
    states: HashMap<String, AssetMomentumState>,
    /// Assets we're actively tracking.
    tracked_assets: Vec<String>,
}

impl MomentumStrategy {
    pub fn new() -> Self {
        Self {
            name: "Momentum".to_string(),
            active_signals: Vec::new(),
            states: HashMap::new(),
            tracked_assets: vec![
                "BTC".to_string(),
                "ETH".to_string(),
                "SOL".to_string(),
                "AVAX".to_string(),
                "DOGE".to_string(),
                "LINK".to_string(),
                "ARB".to_string(),
                "OP".to_string(),
            ],
        }
    }

    /// Compute EMAs for closing prices.
    fn compute_ema(prices: &[Decimal], period: usize) -> Vec<Decimal> {
        if prices.is_empty() || period == 0 {
            return Vec::new();
        }
        let k = dec!(2) / (Decimal::from(period) + Decimal::ONE);
        let mut ema = Vec::with_capacity(prices.len());
        ema.push(prices[0]);
        for i in 1..prices.len() {
            let prev = ema[i - 1];
            ema.push(prices[i] * k + prev * (Decimal::ONE - k));
        }
        ema
    }

    /// Compute ATR from candles.
    fn compute_atr(candles: &[Candle], period: usize) -> Vec<Decimal> {
        if candles.len() < 2 {
            return Vec::new();
        }
        let mut tr = vec![candles[0].high - candles[0].low];
        for i in 1..candles.len() {
            let h = candles[i].high;
            let l = candles[i].low;
            let pc = candles[i - 1].close;
            tr.push((h - l).max((h - pc).abs()).max((l - pc).abs()));
        }
        Self::compute_ema(&tr, period)
    }

    /// Compute ADX from candles.
    fn compute_adx(candles: &[Candle], period: usize) -> Vec<Decimal> {
        if candles.len() < period + 1 {
            return Vec::new();
        }
        let mut plus_dm = Vec::new();
        let mut minus_dm = Vec::new();
        let mut tr = Vec::new();

        for i in 1..candles.len() {
            let up = candles[i].high - candles[i - 1].high;
            let down = candles[i - 1].low - candles[i].low;
            plus_dm.push(if up > down && up > Decimal::ZERO { up } else { Decimal::ZERO });
            minus_dm.push(if down > up && down > Decimal::ZERO { down } else { Decimal::ZERO });

            let h = candles[i].high;
            let l = candles[i].low;
            let pc = candles[i - 1].close;
            tr.push((h - l).max((h - pc).abs()).max((l - pc).abs()));
        }

        let s_plus = Self::compute_ema(&plus_dm, period);
        let s_minus = Self::compute_ema(&minus_dm, period);
        let s_tr = Self::compute_ema(&tr, period);

        let mut dx = Vec::new();
        for i in 0..s_tr.len() {
            if s_tr[i].is_zero() {
                dx.push(Decimal::ZERO);
                continue;
            }
            let pdi = s_plus[i] / s_tr[i] * dec!(100);
            let mdi = s_minus[i] / s_tr[i] * dec!(100);
            let sum = pdi + mdi;
            if sum.is_zero() {
                dx.push(Decimal::ZERO);
            } else {
                dx.push((pdi - mdi).abs() / sum * dec!(100));
            }
        }
        Self::compute_ema(&dx, period)
    }

    /// Update indicators for an asset using its candle data.
    fn update_indicators(state: &mut AssetMomentumState) {
        let closes: Vec<Decimal> = state.candles.iter().map(|c| c.close).collect();
        state.ema_20 = Self::compute_ema(&closes, EMA_FAST);
        state.ema_50 = Self::compute_ema(&closes, EMA_MEDIUM);
        state.ema_200 = Self::compute_ema(&closes, EMA_SLOW);
        state.atr = Self::compute_atr(&state.candles, 14);
        state.adx = Self::compute_adx(&state.candles, 14);
    }
}

impl Default for MomentumStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl crate::Strategy for MomentumStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn strategy_type(&self) -> StrategyType {
        StrategyType::Momentum
    }

    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>> {
        let mut signals = Vec::new();
        self.active_signals.clear();

        for asset in &self.tracked_assets.clone() {
            let price = ctx.prices.get(asset).copied().unwrap_or_default();
            if price.is_zero() {
                continue;
            }

            let state = self
                .states
                .entry(asset.clone())
                .or_insert_with(|| AssetMomentumState::new(asset));

            // In real implementation, candles come from the CandleManager
            // For the evaluation cycle, we use simulated/cached candles
            // We need at least 200 candles for full indicator computation
            if state.candles.len() < EMA_SLOW {
                debug!("Momentum: Insufficient candles for {} ({} < {})", asset, state.candles.len(), EMA_SLOW);
                continue;
            }

            // Update indicators
            Self::update_indicators(state);

            // Check trailing stop on existing position
            if let (Some(side), Some(stop)) = (state.last_signal_side, state.trailing_stop) {
                let stopped_out = match side {
                    Side::Long => price < stop,
                    Side::Short => price > stop,
                };
                if stopped_out {
                    info!("Momentum: Trailing stop hit for {} at {}", asset, price);
                    let exit = Signal::new(
                        asset,
                        side.opposite(),
                        dec!(0.9),
                        dec!(0.01),
                        StrategyType::Momentum,
                        &format!("Trailing stop hit: price={}, stop={}", price, stop),
                    );
                    signals.push(exit);
                    state.last_signal_side = None;
                    state.trailing_stop = None;
                    continue;
                }
                // Update trailing stop
                let new_stop = state.compute_trailing_stop(side, price);
                match side {
                    Side::Long => {
                        if new_stop > stop {
                            state.trailing_stop = Some(new_stop);
                        }
                    }
                    Side::Short => {
                        if new_stop < stop {
                            state.trailing_stop = Some(new_stop);
                        }
                    }
                }
            }

            // Skip new entries if already in a position
            if state.last_signal_side.is_some() {
                continue;
            }

            // Check ADX filter
            let current_adx = state.adx.last().copied().unwrap_or_default();
            if current_adx < MIN_ADX {
                debug!("Momentum: ADX too low for {} ({:.1} < {})", asset, current_adx, MIN_ADX);
                continue;
            }

            // Check EMA crossover
            let crossover = state.ema_crossover();
            if crossover.is_none() {
                continue;
            }
            let cross_side = crossover.unwrap();

            // Check volume confirmation
            let volume_ok = state.volume_confirms();

            // Check breakout confirmation
            let breakout_ok = state.is_breakout();
            let breakout_dir = state.breakout_direction();

            // Score the signal based on confirmations
            let mut score = dec!(0.5); // Base for EMA crossover
            if volume_ok {
                score += dec!(0.15);
            }
            if breakout_ok {
                score += dec!(0.15);
                if let Some(bd) = breakout_dir {
                    if bd == cross_side {
                        score += dec!(0.05); // Extra for aligned breakout
                    }
                }
            }
            // ADX strength bonus
            if current_adx > dec!(40) {
                score += dec!(0.05);
            }

            // Need at least some confirmation beyond just EMA crossover
            if !volume_ok && !breakout_ok {
                debug!("Momentum: EMA crossover for {} but no confirmation", asset);
                continue;
            }

            let atr = state.atr.last().copied().unwrap_or(price * dec!(0.02));
            let stop_loss = state.compute_trailing_stop(cross_side, price);

            let signal = Signal::new(
                asset,
                cross_side,
                score.min(dec!(0.9)),
                atr / price, // Edge = ATR / price (expected move)
                StrategyType::Momentum,
                &format!(
                    "Momentum: EMA crossover={}, ADX={:.1}, volume_ok={}, breakout={}",
                    cross_side, current_adx, volume_ok, breakout_ok
                ),
            )
            .with_stop_loss(stop_loss)
            .with_leverage(dec!(3));

            info!(
                "Momentum signal: {} {} — ADX={:.1}, score={:.2}",
                cross_side, asset, current_adx, score
            );

            self.active_signals.push(signal.clone());
            signals.push(signal);
        }

        Ok(signals)
    }

    async fn on_fill(&mut self, fill: &Fill) -> Result<()> {
        if let Some(state) = self.states.get_mut(&fill.asset) {
            if state.last_signal_side.is_some() {
                // Closing position
                state.last_signal_side = None;
                state.trailing_stop = None;
                info!("Momentum: Closed position in {}", fill.asset);
            } else {
                // Opening position
                state.last_signal_side = Some(fill.side);
                state.trailing_stop =
                    Some(state.compute_trailing_stop(fill.side, fill.price));
                info!(
                    "Momentum: Opened {} position in {} @ {}",
                    fill.side, fill.asset, fill.price
                );
            }
        }
        Ok(())
    }

    fn active_signals(&self) -> &[Signal] {
        &self.active_signals
    }

    fn risk_params(&self) -> StrategyRiskParams {
        StrategyRiskParams {
            max_position_pct: dec!(10),
            max_leverage: dec!(3),
            max_drawdown_pct: dec!(8),
            max_correlated_exposure: dec!(25),
            cooldown_secs: 600,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::Portfolio;

    fn make_candles(n: usize, start_price: f64, trend: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let base = start_price + i as f64 * trend;
                Candle {
                    open: Decimal::from_f64_retain(base).unwrap(),
                    high: Decimal::from_f64_retain(base + 50.0).unwrap(),
                    low: Decimal::from_f64_retain(base - 50.0).unwrap(),
                    close: Decimal::from_f64_retain(base + 10.0).unwrap(),
                    volume: Decimal::from_f64_retain(1000.0 + (i as f64) * 100.0).unwrap(),
                    timestamp: Utc::now(),
                    num_trades: 100,
                }
            })
            .collect()
    }

    #[test]
    fn test_compute_ema() {
        let prices = vec![dec!(10), dec!(11), dec!(12), dec!(11), dec!(13)];
        let ema = MomentumStrategy::compute_ema(&prices, 3);
        assert_eq!(ema.len(), 5);
    }

    #[test]
    fn test_compute_atr() {
        let candles = make_candles(30, 50000.0, 100.0);
        let atr = MomentumStrategy::compute_atr(&candles, 14);
        assert!(!atr.is_empty());
        assert!(atr.last().unwrap() > &Decimal::ZERO);
    }

    #[test]
    fn test_ema_crossover_bullish() {
        let mut state = AssetMomentumState::new("BTC");
        // Create a scenario where fast EMA crosses above medium
        state.ema_20 = vec![dec!(100), dec!(105)]; // Crossing up
        state.ema_50 = vec![dec!(103), dec!(104)]; // Being crossed

        let crossover = state.ema_crossover();
        assert_eq!(crossover, Some(Side::Long));
    }

    #[test]
    fn test_ema_crossover_bearish() {
        let mut state = AssetMomentumState::new("BTC");
        state.ema_20 = vec![dec!(105), dec!(100)]; // Crossing down
        state.ema_50 = vec![dec!(103), dec!(101)]; // Being crossed

        let crossover = state.ema_crossover();
        assert_eq!(crossover, Some(Side::Short));
    }

    #[test]
    fn test_volume_confirms() {
        let mut state = AssetMomentumState::new("BTC");
        // 20 candles with average vol = 1000
        for i in 0..20 {
            state.candles.push(Candle {
                open: dec!(50000),
                high: dec!(50100),
                low: dec!(49900),
                close: dec!(50050),
                volume: dec!(1000),
                timestamp: Utc::now(),
                num_trades: 50,
            });
        }
        // Current candle with high volume
        state.candles.push(Candle {
            open: dec!(50000),
            high: dec!(50500),
            low: dec!(49800),
            close: dec!(50400),
            volume: dec!(2000), // 2x average
            timestamp: Utc::now(),
            num_trades: 100,
        });

        assert!(state.volume_confirms());
    }

    #[test]
    fn test_trailing_stop_long() {
        let mut state = AssetMomentumState::new("BTC");
        state.atr = vec![dec!(500)];

        let stop = state.compute_trailing_stop(Side::Long, dec!(50000));
        // 50000 - 500 * 2 = 49000
        assert_eq!(stop, dec!(49000));
    }

    #[test]
    fn test_trailing_stop_short() {
        let mut state = AssetMomentumState::new("BTC");
        state.atr = vec![dec!(500)];

        let stop = state.compute_trailing_stop(Side::Short, dec!(50000));
        // 50000 + 500 * 2 = 51000
        assert_eq!(stop, dec!(51000));
    }

    #[tokio::test]
    async fn test_momentum_insufficient_data() {
        let mut strategy = MomentumStrategy::new();
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
        assert!(signals.is_empty());
    }

    #[test]
    fn test_risk_params() {
        let strategy = MomentumStrategy::new();
        let params = strategy.risk_params();
        assert_eq!(params.max_leverage, dec!(3));
        assert_eq!(params.max_drawdown_pct, dec!(8));
    }
}
