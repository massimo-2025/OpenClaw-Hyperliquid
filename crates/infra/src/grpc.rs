/// gRPC service for the OpenClaw ↔ Rust bridge.
/// 
/// The proto definitions are compiled separately. This module provides
/// the service implementation that receives LLM signals and exposes
/// portfolio data and emergency controls.

use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Signal received via gRPC from OpenClaw.
#[derive(Debug, Clone)]
pub struct GrpcSignal {
    pub asset: String,
    pub direction: String,
    pub confidence: f64,
    pub edge: f64,
    pub suggested_leverage: f64,
    pub signal_source: String,
    pub reasoning: String,
}

/// Portfolio info for gRPC responses.
#[derive(Debug, Clone)]
pub struct GrpcPortfolioInfo {
    pub nav: f64,
    pub total_pnl: f64,
    pub daily_pnl: f64,
    pub leverage: f64,
    pub margin_ratio: f64,
    pub positions: Vec<GrpcPositionInfo>,
}

#[derive(Debug, Clone)]
pub struct GrpcPositionInfo {
    pub asset: String,
    pub side: String,
    pub size: f64,
    pub entry_price: f64,
    pub mark_price: f64,
    pub unrealized_pnl: f64,
    pub leverage: f64,
    pub strategy: String,
}

/// Signal queue for passing gRPC signals to the strategy engine.
pub struct SignalQueue {
    signals: Arc<Mutex<Vec<GrpcSignal>>>,
}

impl SignalQueue {
    pub fn new() -> Self {
        Self {
            signals: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Push a new signal into the queue.
    pub async fn push(&self, signal: GrpcSignal) {
        let mut queue = self.signals.lock().await;
        info!(
            "gRPC signal queued: {} {} confidence={:.2}",
            signal.direction, signal.asset, signal.confidence
        );
        queue.push(signal);
    }

    /// Drain all pending signals.
    pub async fn drain(&self) -> Vec<GrpcSignal> {
        let mut queue = self.signals.lock().await;
        std::mem::take(&mut *queue)
    }

    /// Get number of pending signals.
    pub async fn pending_count(&self) -> usize {
        self.signals.lock().await.len()
    }
}

impl Default for SignalQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Emergency close result.
#[derive(Debug, Clone)]
pub struct CloseResult {
    pub positions_closed: i32,
    pub realized_pnl: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_signal_queue_push_drain() {
        let queue = SignalQueue::new();

        let signal = GrpcSignal {
            asset: "BTC".to_string(),
            direction: "long".to_string(),
            confidence: 0.85,
            edge: 0.05,
            suggested_leverage: 2.0,
            signal_source: "openclaw".to_string(),
            reasoning: "Bullish signal".to_string(),
        };

        queue.push(signal).await;
        assert_eq!(queue.pending_count().await, 1);

        let signals = queue.drain().await;
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].asset, "BTC");

        assert_eq!(queue.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_signal_queue_multiple() {
        let queue = SignalQueue::new();

        for i in 0..5 {
            queue.push(GrpcSignal {
                asset: format!("ASSET{i}"),
                direction: "long".to_string(),
                confidence: 0.7,
                edge: 0.03,
                suggested_leverage: 1.0,
                signal_source: "test".to_string(),
                reasoning: "Test".to_string(),
            }).await;
        }

        assert_eq!(queue.pending_count().await, 5);
        let signals = queue.drain().await;
        assert_eq!(signals.len(), 5);
    }
}
