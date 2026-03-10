use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::debug;

/// Position correlation matrix tracking.
/// Monitors correlations between held positions to detect concentration risk.
pub struct CorrelationTracker {
    /// Price history per asset for correlation calculation.
    price_history: HashMap<String, Vec<Decimal>>,
    /// Cached correlation matrix (asset_a, asset_b) → correlation.
    correlations: HashMap<(String, String), Decimal>,
    max_history: usize,
}

impl CorrelationTracker {
    pub fn new() -> Self {
        Self {
            price_history: HashMap::new(),
            correlations: HashMap::new(),
            max_history: 100,
        }
    }

    /// Record a price observation for an asset.
    pub fn record_price(&mut self, asset: &str, price: Decimal) {
        let history = self
            .price_history
            .entry(asset.to_string())
            .or_insert_with(Vec::new);
        history.push(price);
        if history.len() > self.max_history {
            history.remove(0);
        }
    }

    /// Compute returns from price history.
    fn compute_returns(prices: &[Decimal]) -> Vec<Decimal> {
        if prices.len() < 2 {
            return Vec::new();
        }
        prices
            .windows(2)
            .filter_map(|w| {
                if w[0].is_zero() {
                    None
                } else {
                    Some((w[1] - w[0]) / w[0])
                }
            })
            .collect()
    }

    /// Compute correlation between two assets.
    pub fn compute_correlation(&self, asset_a: &str, asset_b: &str) -> Option<Decimal> {
        let prices_a = self.price_history.get(asset_a)?;
        let prices_b = self.price_history.get(asset_b)?;

        let returns_a = Self::compute_returns(prices_a);
        let returns_b = Self::compute_returns(prices_b);

        let n = returns_a.len().min(returns_b.len());
        if n < 5 {
            return None;
        }

        let ra = &returns_a[returns_a.len() - n..];
        let rb = &returns_b[returns_b.len() - n..];

        let n_dec = Decimal::from(n);
        let mean_a: Decimal = ra.iter().sum::<Decimal>() / n_dec;
        let mean_b: Decimal = rb.iter().sum::<Decimal>() / n_dec;

        let cov: Decimal = ra
            .iter()
            .zip(rb.iter())
            .map(|(a, b)| (*a - mean_a) * (*b - mean_b))
            .sum::<Decimal>()
            / n_dec;

        let var_a: Decimal = ra
            .iter()
            .map(|a| (*a - mean_a) * (*a - mean_a))
            .sum::<Decimal>()
            / n_dec;

        let var_b: Decimal = rb
            .iter()
            .map(|b| (*b - mean_b) * (*b - mean_b))
            .sum::<Decimal>()
            / n_dec;

        let denom = decimal_sqrt(var_a) * decimal_sqrt(var_b);
        if denom.is_zero() {
            return Some(Decimal::ZERO);
        }

        Some((cov / denom).max(dec!(-1)).min(dec!(1)))
    }

    /// Update the full correlation matrix for all tracked assets.
    pub fn update_matrix(&mut self) {
        let assets: Vec<String> = self.price_history.keys().cloned().collect();
        self.correlations.clear();

        for i in 0..assets.len() {
            for j in (i + 1)..assets.len() {
                if let Some(corr) = self.compute_correlation(&assets[i], &assets[j]) {
                    self.correlations
                        .insert((assets[i].clone(), assets[j].clone()), corr);
                    self.correlations
                        .insert((assets[j].clone(), assets[i].clone()), corr);
                }
            }
        }
    }

    /// Get the correlation between two assets from the cached matrix.
    pub fn get_correlation(&self, asset_a: &str, asset_b: &str) -> Option<Decimal> {
        self.correlations
            .get(&(asset_a.to_string(), asset_b.to_string()))
            .copied()
    }

    /// Find highly correlated pairs above a threshold.
    pub fn high_correlation_pairs(&self, threshold: Decimal) -> Vec<(String, String, Decimal)> {
        let mut pairs = Vec::new();
        let mut seen = std::collections::HashSet::new();

        for ((a, b), &corr) in &self.correlations {
            if corr.abs() >= threshold {
                let key = if a < b {
                    (a.clone(), b.clone())
                } else {
                    (b.clone(), a.clone())
                };
                if seen.insert(key.clone()) {
                    pairs.push((key.0, key.1, corr));
                }
            }
        }

        pairs.sort_by(|a, b| {
            b.2.abs()
                .partial_cmp(&a.2.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        pairs
    }

    /// Compute average correlation of a specific asset with all other tracked assets.
    pub fn average_correlation(&self, asset: &str) -> Decimal {
        let corrs: Vec<Decimal> = self
            .correlations
            .iter()
            .filter(|((a, _), _)| a == asset)
            .map(|(_, &corr)| corr.abs())
            .collect();

        if corrs.is_empty() {
            return Decimal::ZERO;
        }

        corrs.iter().sum::<Decimal>() / Decimal::from(corrs.len())
    }

    /// Detect regime change: sudden spike in average correlation.
    pub fn is_correlation_spike(&self, threshold: Decimal) -> bool {
        let assets: Vec<String> = self.price_history.keys().cloned().collect();
        if assets.len() < 2 {
            return false;
        }

        let total_pairs = assets.len() * (assets.len() - 1) / 2;
        if total_pairs == 0 {
            return false;
        }

        let avg_corr: Decimal = self
            .correlations
            .values()
            .map(|c| c.abs())
            .sum::<Decimal>()
            / Decimal::from(self.correlations.len().max(1));

        avg_corr > threshold
    }

    /// Get number of tracked assets.
    pub fn tracked_count(&self) -> usize {
        self.price_history.len()
    }
}

impl Default for CorrelationTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Approximate square root for Decimal.
fn decimal_sqrt(val: Decimal) -> Decimal {
    if val <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let mut x = val;
    for _ in 0..20 {
        let next = (x + val / x) / dec!(2);
        if (next - x).abs() < dec!(0.0000001) {
            return next;
        }
        x = next;
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_and_compute_returns() {
        let prices = vec![dec!(100), dec!(110), dec!(105), dec!(115)];
        let returns = CorrelationTracker::compute_returns(&prices);
        assert_eq!(returns.len(), 3);
        assert_eq!(returns[0], dec!(0.1)); // 10% up
    }

    #[test]
    fn test_perfect_correlation() {
        let mut tracker = CorrelationTracker::new();
        for i in 0..20 {
            tracker.record_price("A", Decimal::from(100 + i));
            tracker.record_price("B", Decimal::from(200 + i * 2));
        }

        let corr = tracker.compute_correlation("A", "B").unwrap();
        assert!(corr > dec!(0.99));
    }

    #[test]
    fn test_negative_correlation() {
        let mut tracker = CorrelationTracker::new();
        for i in 0..20 {
            tracker.record_price("A", Decimal::from(100 + i));
            tracker.record_price("B", Decimal::from(200 - i));
        }

        let corr = tracker.compute_correlation("A", "B").unwrap();
        assert!(corr < dec!(-0.99));
    }

    #[test]
    fn test_insufficient_data() {
        let mut tracker = CorrelationTracker::new();
        tracker.record_price("A", dec!(100));
        tracker.record_price("B", dec!(200));

        assert!(tracker.compute_correlation("A", "B").is_none());
    }

    #[test]
    fn test_update_matrix() {
        let mut tracker = CorrelationTracker::new();
        for i in 0..20 {
            tracker.record_price("A", Decimal::from(100 + i));
            tracker.record_price("B", Decimal::from(200 + i));
            tracker.record_price("C", Decimal::from(300 - i));
        }

        tracker.update_matrix();
        assert!(tracker.get_correlation("A", "B").is_some());
        assert!(tracker.get_correlation("A", "C").is_some());
    }

    #[test]
    fn test_high_correlation_pairs() {
        let mut tracker = CorrelationTracker::new();
        for i in 0..20 {
            tracker.record_price("A", Decimal::from(100 + i));
            tracker.record_price("B", Decimal::from(200 + i));
        }
        tracker.update_matrix();

        let pairs = tracker.high_correlation_pairs(dec!(0.9));
        assert!(!pairs.is_empty());
    }

    #[test]
    fn test_average_correlation() {
        let mut tracker = CorrelationTracker::new();
        for i in 0..20 {
            tracker.record_price("A", Decimal::from(100 + i));
            tracker.record_price("B", Decimal::from(200 + i));
            tracker.record_price("C", Decimal::from(300 + i));
        }
        tracker.update_matrix();

        let avg = tracker.average_correlation("A");
        assert!(avg > Decimal::ZERO);
    }

    #[test]
    fn test_tracked_count() {
        let mut tracker = CorrelationTracker::new();
        tracker.record_price("A", dec!(100));
        tracker.record_price("B", dec!(200));
        assert_eq!(tracker.tracked_count(), 2);
    }
}
