pub mod basis_trade;
pub mod funding_arb;
pub mod liquidation;
pub mod llm_signal;
pub mod market_maker;
pub mod momentum;
pub mod stat_arb;

use anyhow::Result;
use async_trait::async_trait;
use hl_core::{Fill, MarketContext, Signal, StrategyRiskParams, StrategyType};

/// Core strategy trait that all trading strategies must implement.
#[async_trait]
pub trait Strategy: Send + Sync {
    /// Human-readable strategy name.
    fn name(&self) -> &str;

    /// Strategy type enum for categorization.
    fn strategy_type(&self) -> StrategyType;

    /// Evaluate current market conditions and produce signals.
    /// Called on every scan interval.
    async fn evaluate(&mut self, ctx: &MarketContext) -> Result<Vec<Signal>>;

    /// Callback when a fill is received for an order placed by this strategy.
    async fn on_fill(&mut self, fill: &Fill) -> Result<()>;

    /// Get currently active (unexpired) signals.
    fn active_signals(&self) -> &[Signal];

    /// Risk parameters specific to this strategy.
    fn risk_params(&self) -> StrategyRiskParams;
}

/// Registry of all available strategies.
pub struct StrategyRegistry {
    strategies: Vec<Box<dyn Strategy>>,
}

impl StrategyRegistry {
    pub fn new() -> Self {
        Self {
            strategies: Vec::new(),
        }
    }

    /// Register a strategy.
    pub fn register(&mut self, strategy: Box<dyn Strategy>) {
        tracing::info!("Registered strategy: {}", strategy.name());
        self.strategies.push(strategy);
    }

    /// Create all default strategies.
    pub fn create_all_defaults() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(funding_arb::FundingArbStrategy::new()));
        registry.register(Box::new(basis_trade::BasisTradeStrategy::new()));
        registry.register(Box::new(market_maker::MarketMakerStrategy::new()));
        registry.register(Box::new(liquidation::LiquidationStrategy::new()));
        registry.register(Box::new(stat_arb::StatArbStrategy::new()));
        registry.register(Box::new(llm_signal::LlmSignalStrategy::new()));
        registry.register(Box::new(momentum::MomentumStrategy::new()));
        registry
    }

    /// Create strategies by name filter.
    pub fn create_filtered(names: &[String]) -> Self {
        if names.iter().any(|n| n == "all") {
            return Self::create_all_defaults();
        }

        let mut registry = Self::new();
        for name in names {
            match name.as_str() {
                "funding_arb" => {
                    registry.register(Box::new(funding_arb::FundingArbStrategy::new()))
                }
                "basis_trade" => {
                    registry.register(Box::new(basis_trade::BasisTradeStrategy::new()))
                }
                "market_maker" => {
                    registry.register(Box::new(market_maker::MarketMakerStrategy::new()))
                }
                "liquidation" => {
                    registry.register(Box::new(liquidation::LiquidationStrategy::new()))
                }
                "stat_arb" => registry.register(Box::new(stat_arb::StatArbStrategy::new())),
                "llm_signal" => {
                    registry.register(Box::new(llm_signal::LlmSignalStrategy::new()))
                }
                "momentum" => registry.register(Box::new(momentum::MomentumStrategy::new())),
                other => tracing::warn!("Unknown strategy: {other}"),
            }
        }
        registry
    }

    /// Get all strategies as mutable references.
    pub fn strategies_mut(&mut self) -> &mut [Box<dyn Strategy>] {
        &mut self.strategies
    }

    /// Get all strategies as immutable references.
    pub fn strategies(&self) -> &[Box<dyn Strategy>] {
        &self.strategies
    }

    /// Get number of registered strategies.
    pub fn count(&self) -> usize {
        self.strategies.len()
    }
}

impl Default for StrategyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_create_all() {
        let registry = StrategyRegistry::create_all_defaults();
        assert_eq!(registry.count(), 7);
    }

    #[test]
    fn test_registry_filtered() {
        let registry =
            StrategyRegistry::create_filtered(&["funding_arb".to_string(), "momentum".to_string()]);
        assert_eq!(registry.count(), 2);
    }

    #[test]
    fn test_registry_all_keyword() {
        let registry = StrategyRegistry::create_filtered(&["all".to_string()]);
        assert_eq!(registry.count(), 7);
    }

    #[test]
    fn test_registry_unknown_strategy() {
        let registry = StrategyRegistry::create_filtered(&["nonexistent".to_string()]);
        assert_eq!(registry.count(), 0);
    }
}
