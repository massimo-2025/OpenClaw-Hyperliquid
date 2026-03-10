use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

/// Top-level application configuration loaded from environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub hyperliquid: HyperliquidConfig,
    pub binance: Option<BinanceConfig>,
    pub telegram: Option<TelegramConfig>,
    pub neo4j: Option<Neo4jConfig>,
    pub risk: RiskConfig,
    pub trading: TradingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyperliquidConfig {
    pub wallet_address: String,
    pub private_key: String,
    pub testnet: bool,
    pub info_url: String,
    pub exchange_url: String,
    pub ws_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinanceConfig {
    pub api_key: String,
    pub secret: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
    pub max_per_market_5min: usize,
    pub max_per_hour: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neo4jConfig {
    pub uri: String,
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub max_portfolio_leverage: Decimal,
    pub max_position_pct: Decimal,
    pub max_correlated_pct: Decimal,
    pub daily_drawdown_limit: Decimal,
    pub total_drawdown_limit: Decimal,
    pub max_leverage_per_position: Decimal,
    pub liquidation_buffer_multiplier: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradingConfig {
    pub scan_interval_secs: u64,
    pub paper_trade: bool,
    pub starting_balance: Decimal,
    pub enabled_strategies: Vec<String>,
}

fn env_var(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("Missing environment variable: {key}"))
}

fn env_var_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_decimal(key: &str, default: &str) -> Decimal {
    Decimal::from_str(&env_var_or(key, default)).unwrap_or_else(|_| Decimal::from_str(default).unwrap())
}

impl AppConfig {
    /// Load configuration from environment variables.
    /// Call `dotenvy::dotenv().ok()` before this if you want .env support.
    pub fn from_env() -> Result<Self> {
        let testnet = env_var_or("HYPERLIQUID_TESTNET", "false")
            .parse::<bool>()
            .unwrap_or(false);

        let (info_url, exchange_url, ws_url) = if testnet {
            (
                "https://api.hyperliquid-testnet.xyz/info".to_string(),
                "https://api.hyperliquid-testnet.xyz/exchange".to_string(),
                "wss://api.hyperliquid-testnet.xyz/ws".to_string(),
            )
        } else {
            (
                "https://api.hyperliquid.xyz/info".to_string(),
                "https://api.hyperliquid.xyz/exchange".to_string(),
                "wss://api.hyperliquid.xyz/ws".to_string(),
            )
        };

        let hyperliquid = HyperliquidConfig {
            wallet_address: env_var_or("HYPERLIQUID_WALLET_ADDRESS", ""),
            private_key: env_var_or("HYPERLIQUID_PRIVATE_KEY", ""),
            testnet,
            info_url,
            exchange_url,
            ws_url,
        };

        let binance = match (
            std::env::var("BINANCE_API_KEY"),
            std::env::var("BINANCE_SECRET"),
        ) {
            (Ok(key), Ok(secret)) if !key.is_empty() && !secret.is_empty() => {
                Some(BinanceConfig {
                    api_key: key,
                    secret,
                    base_url: "https://fapi.binance.com".to_string(),
                })
            }
            _ => None,
        };

        let telegram = match (
            std::env::var("TELEGRAM_BOT_TOKEN"),
            std::env::var("TELEGRAM_CHAT_ID"),
        ) {
            (Ok(token), Ok(chat_id)) if !token.is_empty() && !chat_id.is_empty() => {
                Some(TelegramConfig {
                    bot_token: token,
                    chat_id,
                    max_per_market_5min: 1,
                    max_per_hour: 20,
                })
            }
            _ => None,
        };

        let neo4j = match (
            std::env::var("NEO4J_URI"),
            std::env::var("NEO4J_USER"),
            std::env::var("NEO4J_PASSWORD"),
        ) {
            (Ok(uri), Ok(user), Ok(password)) if !uri.is_empty() => {
                Some(Neo4jConfig { uri, user, password })
            }
            _ => None,
        };

        let risk = RiskConfig {
            max_portfolio_leverage: env_decimal("MAX_PORTFOLIO_LEVERAGE", "3.0"),
            max_position_pct: env_decimal("MAX_POSITION_PCT", "15.0"),
            max_correlated_pct: env_decimal("MAX_CORRELATED_PCT", "30.0"),
            daily_drawdown_limit: env_decimal("DAILY_DRAWDOWN_LIMIT", "10.0"),
            total_drawdown_limit: env_decimal("TOTAL_DRAWDOWN_LIMIT", "20.0"),
            max_leverage_per_position: env_decimal("MAX_LEVERAGE_PER_POSITION", "5.0"),
            liquidation_buffer_multiplier: env_decimal("LIQUIDATION_BUFFER_MULTIPLIER", "2.0"),
        };

        let trading = TradingConfig {
            scan_interval_secs: env_var_or("SCAN_INTERVAL_SECS", "10")
                .parse()
                .unwrap_or(10),
            paper_trade: true,
            starting_balance: env_decimal("STARTING_BALANCE", "1000"),
            enabled_strategies: vec!["all".to_string()],
        };

        Ok(Self {
            hyperliquid,
            binance,
            telegram,
            neo4j,
            risk,
            trading,
        })
    }

    /// Check if credentials are present for live trading.
    pub fn has_live_credentials(&self) -> bool {
        !self.hyperliquid.wallet_address.is_empty() && !self.hyperliquid.private_key.is_empty()
    }
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self {
            max_portfolio_leverage: Decimal::new(3, 0),
            max_position_pct: Decimal::new(15, 0),
            max_correlated_pct: Decimal::new(30, 0),
            daily_drawdown_limit: Decimal::new(10, 0),
            total_drawdown_limit: Decimal::new(20, 0),
            max_leverage_per_position: Decimal::new(5, 0),
            liquidation_buffer_multiplier: Decimal::new(2, 0),
        }
    }
}

impl Default for TradingConfig {
    fn default() -> Self {
        Self {
            scan_interval_secs: 10,
            paper_trade: true,
            starting_balance: Decimal::new(1000, 0),
            enabled_strategies: vec!["all".to_string()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_risk_config() {
        let risk = RiskConfig::default();
        assert_eq!(risk.max_portfolio_leverage, Decimal::new(3, 0));
        assert_eq!(risk.max_position_pct, Decimal::new(15, 0));
        assert_eq!(risk.daily_drawdown_limit, Decimal::new(10, 0));
        assert_eq!(risk.total_drawdown_limit, Decimal::new(20, 0));
    }

    #[test]
    fn test_default_trading_config() {
        let trading = TradingConfig::default();
        assert_eq!(trading.scan_interval_secs, 10);
        assert!(trading.paper_trade);
        assert_eq!(trading.starting_balance, Decimal::new(1000, 0));
    }

    #[test]
    fn test_config_from_env_defaults() {
        // Clear any existing env vars to test defaults
        std::env::remove_var("HYPERLIQUID_WALLET_ADDRESS");
        std::env::remove_var("HYPERLIQUID_PRIVATE_KEY");
        let config = AppConfig::from_env().unwrap();
        assert!(!config.has_live_credentials());
        assert!(config.binance.is_none());
        assert!(config.telegram.is_none());
    }
}
