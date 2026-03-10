use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use hl_core::AlertType;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

/// Rate limit: max messages per market per 5 minutes.
const MAX_PER_MARKET_5MIN: usize = 1;
/// Rate limit: max total messages per hour.
const MAX_PER_HOUR: usize = 20;

/// Telegram Bot API alerter with rate limiting.
pub struct TelegramAlerter {
    client: Client,
    bot_token: String,
    chat_id: String,
    /// Rate limiting: (market, timestamp) of recent sends.
    market_sends: Arc<Mutex<HashMap<String, Vec<DateTime<Utc>>>>>,
    /// Global rate limiting: timestamps of all sends.
    global_sends: Arc<Mutex<Vec<DateTime<Utc>>>>,
    enabled: bool,
}

impl TelegramAlerter {
    pub fn new(bot_token: &str, chat_id: &str) -> Self {
        let enabled = !bot_token.is_empty() && !chat_id.is_empty();
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            bot_token: bot_token.to_string(),
            chat_id: chat_id.to_string(),
            market_sends: Arc::new(Mutex::new(HashMap::new())),
            global_sends: Arc::new(Mutex::new(Vec::new())),
            enabled,
        }
    }

    pub fn disabled() -> Self {
        Self::new("", "")
    }

    /// Send an alert if rate limits permit.
    pub async fn send_alert(&self, alert: &AlertType) -> Result<bool> {
        if !self.enabled {
            debug!("Telegram disabled, skipping alert");
            return Ok(false);
        }

        let market = self.extract_market(alert);

        // Check rate limits
        if !self.check_rate_limit(&market).await {
            debug!("Rate limited, skipping alert for {}", market);
            return Ok(false);
        }

        let text = alert.format_telegram();
        self.send_message(&text).await?;

        // Record the send
        self.record_send(&market).await;

        Ok(true)
    }

    /// Send a raw text message (bypasses rate limiting).
    pub async fn send_message(&self, text: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let url = format!(
            "https://api.telegram.org/bot{}/sendMessage",
            self.bot_token
        );

        let body = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true
        });

        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Failed to send Telegram message")?;

        if !resp.status().is_success() {
            let error_body = resp.text().await.unwrap_or_default();
            warn!("Telegram API error: {}", error_body);
        } else {
            debug!("Telegram message sent");
        }

        Ok(())
    }

    /// Check rate limits for a market.
    async fn check_rate_limit(&self, market: &str) -> bool {
        let now = Utc::now();
        let five_min_ago = now - chrono::Duration::minutes(5);
        let one_hour_ago = now - chrono::Duration::hours(1);

        // Check per-market limit
        let market_sends = self.market_sends.lock().await;
        if let Some(sends) = market_sends.get(market) {
            let recent = sends.iter().filter(|&&t| t > five_min_ago).count();
            if recent >= MAX_PER_MARKET_5MIN {
                return false;
            }
        }

        // Check global limit
        let global_sends = self.global_sends.lock().await;
        let recent_global = global_sends.iter().filter(|&&t| t > one_hour_ago).count();
        if recent_global >= MAX_PER_HOUR {
            return false;
        }

        true
    }

    /// Record a send for rate limiting.
    async fn record_send(&self, market: &str) {
        let now = Utc::now();

        let mut market_sends = self.market_sends.lock().await;
        market_sends
            .entry(market.to_string())
            .or_insert_with(Vec::new)
            .push(now);

        let mut global_sends = self.global_sends.lock().await;
        global_sends.push(now);

        // Cleanup old entries
        let cutoff = now - chrono::Duration::hours(2);
        for sends in market_sends.values_mut() {
            sends.retain(|&t| t > cutoff);
        }
        global_sends.retain(|&t| t > cutoff);
    }

    /// Extract market name from alert type.
    fn extract_market(&self, alert: &AlertType) -> String {
        match alert {
            AlertType::TradeExecuted { asset, .. } => asset.clone(),
            AlertType::FundingOpportunity { asset, .. } => asset.clone(),
            AlertType::PositionClosed { asset, .. } => asset.clone(),
            AlertType::RiskBreach { breaker, .. } => breaker.clone(),
            AlertType::CircuitBreakerTripped { breaker, .. } => breaker.clone(),
            AlertType::DailySummary { .. } => "daily_summary".to_string(),
        }
    }

    /// Is the alerter enabled?
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hl_core::Side;
    use rust_decimal_macros::dec;

    #[test]
    fn test_alerter_disabled() {
        let alerter = TelegramAlerter::disabled();
        assert!(!alerter.is_enabled());
    }

    #[test]
    fn test_alerter_enabled() {
        let alerter = TelegramAlerter::new("test-token", "12345");
        assert!(alerter.is_enabled());
    }

    #[tokio::test]
    async fn test_rate_limit_check() {
        let alerter = TelegramAlerter::new("test", "12345");
        // First check should pass
        assert!(alerter.check_rate_limit("BTC").await);

        // Record a send
        alerter.record_send("BTC").await;

        // Second check for same market should fail (1 per 5 min)
        assert!(!alerter.check_rate_limit("BTC").await);

        // Different market should pass
        assert!(alerter.check_rate_limit("ETH").await);
    }

    #[test]
    fn test_extract_market() {
        let alerter = TelegramAlerter::disabled();

        let alert = AlertType::TradeExecuted {
            asset: "BTC".to_string(),
            side: Side::Long,
            size: dec!(1),
            price: dec!(50000),
            strategy: "test".to_string(),
        };
        assert_eq!(alerter.extract_market(&alert), "BTC");

        let alert = AlertType::DailySummary {
            nav: dec!(10000),
            daily_pnl: dec!(500),
            positions: 3,
            active_strategies: 5,
        };
        assert_eq!(alerter.extract_market(&alert), "daily_summary");
    }

    #[tokio::test]
    async fn test_disabled_send_alert() {
        let alerter = TelegramAlerter::disabled();
        let alert = AlertType::TradeExecuted {
            asset: "BTC".to_string(),
            side: Side::Long,
            size: dec!(1),
            price: dec!(50000),
            strategy: "test".to_string(),
        };
        let sent = alerter.send_alert(&alert).await.unwrap();
        assert!(!sent);
    }
}
