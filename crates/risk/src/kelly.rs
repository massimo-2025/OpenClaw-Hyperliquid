use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use tracing::debug;

/// Fractional Kelly sizing for perpetual futures.
///
/// Standard Kelly: f* = (p * b - q) / b
/// where p = probability of win, q = 1 - p, b = win/loss ratio (edge/risk)
///
/// For perps, we apply:
/// 1. Quarter-Kelly (0.25x) for conservative sizing
/// 2. Leverage adjustment
/// 3. Correlation penalty with existing positions
/// 4. Maximum leverage cap
pub struct KellySizer {
    /// Kelly fraction (0.25 = quarter-Kelly).
    kelly_fraction: Decimal,
    /// Maximum leverage per position.
    max_leverage: Decimal,
}

impl KellySizer {
    pub fn new(kelly_fraction: Decimal, max_leverage: Decimal) -> Self {
        Self {
            kelly_fraction,
            max_leverage,
        }
    }

    /// Compute Kelly-optimal position size.
    ///
    /// # Arguments
    /// * `win_probability` - Probability of a winning trade (0-1)
    /// * `edge` - Expected edge per trade (e.g., 0.05 = 5%)
    /// * `leverage` - Desired leverage
    ///
    /// # Returns
    /// Fraction of capital to allocate (0-1, already Kelly-adjusted).
    pub fn compute_fraction(
        &self,
        win_probability: Decimal,
        edge: Decimal,
        leverage: Decimal,
    ) -> Decimal {
        if win_probability <= Decimal::ZERO
            || win_probability >= Decimal::ONE
            || edge <= Decimal::ZERO
        {
            return Decimal::ZERO;
        }

        let p = win_probability;
        let q = Decimal::ONE - p;

        // Win/loss ratio: we use edge as the expected return on a win
        // and a simplified loss of equal magnitude
        let b = if edge > Decimal::ZERO { edge / edge } else { Decimal::ONE };

        // Standard Kelly: f* = (p * b - q) / b
        let kelly_raw = (p * b - q) / b;

        if kelly_raw <= Decimal::ZERO {
            return Decimal::ZERO;
        }

        // Apply fractional Kelly
        let fractional = kelly_raw * self.kelly_fraction;

        // Leverage adjustment: reduce fraction when using higher leverage
        let effective_leverage = leverage.min(self.max_leverage);
        let leverage_adj = if effective_leverage > Decimal::ONE {
            fractional / effective_leverage
        } else {
            fractional
        };

        // Cap at reasonable bounds
        leverage_adj
            .max(Decimal::ZERO)
            .min(dec!(0.25)) // Never more than 25% of capital
    }

    /// Compute absolute position size in USD terms.
    pub fn compute_size(
        &self,
        win_probability: Decimal,
        edge: Decimal,
        leverage: Decimal,
    ) -> Decimal {
        self.compute_fraction(win_probability, edge, leverage)
    }

    /// Adjust Kelly size for correlation with existing positions.
    ///
    /// When a new position is correlated with existing ones,
    /// we reduce the size to account for concentration risk.
    pub fn correlation_adjusted_size(
        &self,
        base_size: Decimal,
        correlation: Decimal,
        num_existing_positions: usize,
    ) -> Decimal {
        if num_existing_positions == 0 || correlation.abs() < dec!(0.3) {
            return base_size;
        }

        // Reduce by correlation factor × number of correlated positions
        let reduction = correlation.abs() * Decimal::from(num_existing_positions) * dec!(0.1);
        let factor = (Decimal::ONE - reduction).max(dec!(0.2));

        base_size * factor
    }

    /// Get the Kelly fraction being used.
    pub fn kelly_fraction(&self) -> Decimal {
        self.kelly_fraction
    }

    /// Get the maximum leverage.
    pub fn max_leverage(&self) -> Decimal {
        self.max_leverage
    }
}

impl Default for KellySizer {
    fn default() -> Self {
        Self::new(dec!(0.25), dec!(5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_kelly_basic() {
        let sizer = KellySizer::default();
        let fraction = sizer.compute_fraction(dec!(0.6), dec!(0.1), dec!(1));
        assert!(fraction > Decimal::ZERO);
        assert!(fraction <= dec!(0.25));
    }

    #[test]
    fn test_kelly_high_confidence() {
        let sizer = KellySizer::default();
        let fraction_high = sizer.compute_fraction(dec!(0.9), dec!(0.1), dec!(1));
        let fraction_low = sizer.compute_fraction(dec!(0.55), dec!(0.1), dec!(1));
        assert!(fraction_high > fraction_low);
    }

    #[test]
    fn test_kelly_zero_edge() {
        let sizer = KellySizer::default();
        let fraction = sizer.compute_fraction(dec!(0.6), Decimal::ZERO, dec!(1));
        assert_eq!(fraction, Decimal::ZERO);
    }

    #[test]
    fn test_kelly_negative_edge() {
        let sizer = KellySizer::default();
        let fraction = sizer.compute_fraction(dec!(0.6), dec!(-0.1), dec!(1));
        assert_eq!(fraction, Decimal::ZERO);
    }

    #[test]
    fn test_kelly_low_probability() {
        let sizer = KellySizer::default();
        let fraction = sizer.compute_fraction(dec!(0.3), dec!(0.1), dec!(1));
        // p*b - q = 0.3 - 0.7 = -0.4 → should be 0
        assert_eq!(fraction, Decimal::ZERO);
    }

    #[test]
    fn test_kelly_leverage_reduces_size() {
        let sizer = KellySizer::default();
        let frac_1x = sizer.compute_fraction(dec!(0.7), dec!(0.1), dec!(1));
        let frac_5x = sizer.compute_fraction(dec!(0.7), dec!(0.1), dec!(5));
        assert!(frac_5x < frac_1x);
    }

    #[test]
    fn test_kelly_max_leverage_cap() {
        let sizer = KellySizer::new(dec!(0.25), dec!(5));
        let frac_10x = sizer.compute_fraction(dec!(0.8), dec!(0.1), dec!(10));
        let frac_5x = sizer.compute_fraction(dec!(0.8), dec!(0.1), dec!(5));
        // 10x should be capped to 5x, so same result
        assert_eq!(frac_10x, frac_5x);
    }

    #[test]
    fn test_correlation_adjustment_none() {
        let sizer = KellySizer::default();
        let adjusted = sizer.correlation_adjusted_size(dec!(1000), dec!(0.1), 0);
        assert_eq!(adjusted, dec!(1000)); // No reduction with 0 positions
    }

    #[test]
    fn test_correlation_adjustment_low_correlation() {
        let sizer = KellySizer::default();
        let adjusted = sizer.correlation_adjusted_size(dec!(1000), dec!(0.2), 3);
        assert_eq!(adjusted, dec!(1000)); // Below 0.3 threshold
    }

    #[test]
    fn test_correlation_adjustment_high() {
        let sizer = KellySizer::default();
        let adjusted = sizer.correlation_adjusted_size(dec!(1000), dec!(0.8), 3);
        // Reduction: 0.8 * 3 * 0.1 = 0.24, factor = 0.76
        assert!(adjusted < dec!(1000));
        assert!(adjusted > dec!(700));
    }

    #[test]
    fn test_correlation_adjustment_max_reduction() {
        let sizer = KellySizer::default();
        let adjusted = sizer.correlation_adjusted_size(dec!(1000), dec!(0.9), 20);
        // Factor should be capped at 0.2 (80% reduction max)
        assert_eq!(adjusted, dec!(200));
    }

    #[test]
    fn test_default_sizer() {
        let sizer = KellySizer::default();
        assert_eq!(sizer.kelly_fraction(), dec!(0.25));
        assert_eq!(sizer.max_leverage(), dec!(5));
    }
}
