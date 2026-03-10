use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

// ─── Side ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub fn opposite(&self) -> Self {
        match self {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        }
    }

    pub fn sign(&self) -> Decimal {
        match self {
            Side::Long => Decimal::ONE,
            Side::Short => Decimal::NEGATIVE_ONE,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Side::Long => write!(f, "Long"),
            Side::Short => write!(f, "Short"),
        }
    }
}

// ─── Order Types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderType {
    Market,
    Limit,
    LimitIoc,
    LimitAlo,
    StopMarket,
    TakeProfit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Alo,
}

// ─── Order ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub client_id: String,
    pub asset: String,
    pub side: Side,
    pub order_type: OrderType,
    pub size: Decimal,
    pub price: Option<Decimal>,
    pub trigger_price: Option<Decimal>,
    pub reduce_only: bool,
    pub status: OrderStatus,
    pub filled_size: Decimal,
    pub avg_fill_price: Option<Decimal>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub strategy: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    Open,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

impl Order {
    pub fn new_market(asset: &str, side: Side, size: Decimal, strategy: &str) -> Self {
        let now = Utc::now();
        Self {
            id: String::new(),
            client_id: Uuid::new_v4().to_string(),
            asset: asset.to_string(),
            side,
            order_type: OrderType::Market,
            size,
            price: None,
            trigger_price: None,
            reduce_only: false,
            status: OrderStatus::Pending,
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            created_at: now,
            updated_at: now,
            strategy: strategy.to_string(),
        }
    }

    pub fn new_limit(
        asset: &str,
        side: Side,
        size: Decimal,
        price: Decimal,
        strategy: &str,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: String::new(),
            client_id: Uuid::new_v4().to_string(),
            asset: asset.to_string(),
            side,
            order_type: OrderType::Limit,
            size,
            price: Some(price),
            trigger_price: None,
            reduce_only: false,
            status: OrderStatus::Pending,
            filled_size: Decimal::ZERO,
            avg_fill_price: None,
            created_at: now,
            updated_at: now,
            strategy: strategy.to_string(),
        }
    }

    pub fn remaining_size(&self) -> Decimal {
        self.size - self.filled_size
    }

    pub fn is_complete(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Filled | OrderStatus::Cancelled | OrderStatus::Rejected
        )
    }
}

// ─── Fill ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fill {
    pub order_id: String,
    pub asset: String,
    pub side: Side,
    pub size: Decimal,
    pub price: Decimal,
    pub fee: Decimal,
    pub timestamp: DateTime<Utc>,
    pub strategy: String,
    pub is_maker: bool,
}

// ─── Position ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub asset: String,
    pub side: Side,
    pub size: Decimal,
    pub entry_price: Decimal,
    pub mark_price: Decimal,
    pub liquidation_price: Option<Decimal>,
    pub unrealized_pnl: Decimal,
    pub realized_pnl: Decimal,
    pub leverage: Decimal,
    pub margin_used: Decimal,
    pub strategy: String,
    pub opened_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub cumulative_funding: Decimal,
}

impl Position {
    pub fn notional_value(&self) -> Decimal {
        self.size * self.mark_price
    }

    pub fn update_mark_price(&mut self, mark: Decimal) {
        self.mark_price = mark;
        let direction = self.side.sign();
        self.unrealized_pnl = direction * self.size * (mark - self.entry_price);
        self.updated_at = Utc::now();
    }

    pub fn total_pnl(&self) -> Decimal {
        self.unrealized_pnl + self.realized_pnl + self.cumulative_funding
    }
}

// ─── Portfolio ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    pub nav: Decimal,
    pub cash: Decimal,
    pub total_margin_used: Decimal,
    pub total_unrealized_pnl: Decimal,
    pub total_realized_pnl: Decimal,
    pub positions: Vec<Position>,
    pub leverage: Decimal,
    pub margin_ratio: Decimal,
    pub high_water_mark: Decimal,
    pub daily_starting_nav: Decimal,
    pub updated_at: DateTime<Utc>,
}

impl Portfolio {
    pub fn new(starting_balance: Decimal) -> Self {
        let now = Utc::now();
        Self {
            nav: starting_balance,
            cash: starting_balance,
            total_margin_used: Decimal::ZERO,
            total_unrealized_pnl: Decimal::ZERO,
            total_realized_pnl: Decimal::ZERO,
            positions: Vec::new(),
            leverage: Decimal::ZERO,
            margin_ratio: Decimal::ONE,
            high_water_mark: starting_balance,
            daily_starting_nav: starting_balance,
            updated_at: now,
        }
    }

    pub fn daily_pnl(&self) -> Decimal {
        self.nav - self.daily_starting_nav
    }

    pub fn daily_return_pct(&self) -> Decimal {
        if self.daily_starting_nav.is_zero() {
            return Decimal::ZERO;
        }
        (self.nav - self.daily_starting_nav) / self.daily_starting_nav * Decimal::ONE_HUNDRED
    }

    pub fn drawdown_from_hwm(&self) -> Decimal {
        if self.high_water_mark.is_zero() {
            return Decimal::ZERO;
        }
        (self.high_water_mark - self.nav) / self.high_water_mark * Decimal::ONE_HUNDRED
    }

    pub fn recalculate(&mut self) {
        self.total_unrealized_pnl = self.positions.iter().map(|p| p.unrealized_pnl).sum();
        self.total_margin_used = self.positions.iter().map(|p| p.margin_used).sum();
        self.nav = self.cash + self.total_unrealized_pnl;
        if self.nav > self.high_water_mark {
            self.high_water_mark = self.nav;
        }
        let total_notional: Decimal = self.positions.iter().map(|p| p.notional_value()).sum();
        self.leverage = if self.nav.is_zero() {
            Decimal::ZERO
        } else {
            total_notional / self.nav
        };
        self.margin_ratio = if self.total_margin_used.is_zero() {
            Decimal::ONE
        } else {
            self.nav / self.total_margin_used
        };
        self.updated_at = Utc::now();
    }

    pub fn position_for_asset(&self, asset: &str) -> Option<&Position> {
        self.positions.iter().find(|p| p.asset == asset)
    }

    pub fn position_for_asset_mut(&mut self, asset: &str) -> Option<&mut Position> {
        self.positions.iter_mut().find(|p| p.asset == asset)
    }
}

// ─── Strategy Types ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StrategyType {
    FundingArb,
    BasisTrade,
    MarketMaker,
    Liquidation,
    StatArb,
    LlmSignal,
    Momentum,
}

impl fmt::Display for StrategyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StrategyType::FundingArb => write!(f, "FundingArb"),
            StrategyType::BasisTrade => write!(f, "BasisTrade"),
            StrategyType::MarketMaker => write!(f, "MarketMaker"),
            StrategyType::Liquidation => write!(f, "Liquidation"),
            StrategyType::StatArb => write!(f, "StatArb"),
            StrategyType::LlmSignal => write!(f, "LlmSignal"),
            StrategyType::Momentum => write!(f, "Momentum"),
        }
    }
}

// ─── Signal ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signal {
    pub id: String,
    pub asset: String,
    pub side: Side,
    pub confidence: Decimal,
    pub edge: Decimal,
    pub suggested_size: Decimal,
    pub suggested_leverage: Decimal,
    pub strategy: StrategyType,
    pub reason: String,
    pub timestamp: DateTime<Utc>,
    pub expiry: Option<DateTime<Utc>>,
    pub stop_loss: Option<Decimal>,
    pub take_profit: Option<Decimal>,
}

impl Signal {
    pub fn new(
        asset: &str,
        side: Side,
        confidence: Decimal,
        edge: Decimal,
        strategy: StrategyType,
        reason: &str,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            asset: asset.to_string(),
            side,
            confidence,
            edge,
            suggested_size: Decimal::ZERO,
            suggested_leverage: Decimal::ONE,
            strategy,
            reason: reason.to_string(),
            timestamp: Utc::now(),
            expiry: None,
            stop_loss: None,
            take_profit: None,
        }
    }

    pub fn with_size(mut self, size: Decimal) -> Self {
        self.suggested_size = size;
        self
    }

    pub fn with_leverage(mut self, leverage: Decimal) -> Self {
        self.suggested_leverage = leverage;
        self
    }

    pub fn with_stop_loss(mut self, sl: Decimal) -> Self {
        self.stop_loss = Some(sl);
        self
    }

    pub fn with_take_profit(mut self, tp: Decimal) -> Self {
        self.take_profit = Some(tp);
        self
    }

    pub fn is_expired(&self) -> bool {
        if let Some(expiry) = self.expiry {
            Utc::now() > expiry
        } else {
            false
        }
    }
}

// ─── Strategy Risk Params ───────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyRiskParams {
    pub max_position_pct: Decimal,
    pub max_leverage: Decimal,
    pub max_drawdown_pct: Decimal,
    pub max_correlated_exposure: Decimal,
    pub cooldown_secs: u64,
}

impl Default for StrategyRiskParams {
    fn default() -> Self {
        Self {
            max_position_pct: Decimal::new(15, 0),
            max_leverage: Decimal::new(5, 0),
            max_drawdown_pct: Decimal::new(10, 0),
            max_correlated_exposure: Decimal::new(30, 0),
            cooldown_secs: 3600,
        }
    }
}

// ─── Market Context ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketContext {
    pub assets: Vec<AssetInfo>,
    pub prices: std::collections::HashMap<String, Decimal>,
    pub funding_rates: std::collections::HashMap<String, Decimal>,
    pub orderbooks: std::collections::HashMap<String, OrderbookSnapshot>,
    pub portfolio: Portfolio,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetInfo {
    pub name: String,
    pub sz_decimals: u32,
    pub max_leverage: u32,
    pub mark_price: Decimal,
    pub oracle_price: Decimal,
    pub funding_rate: Decimal,
    pub open_interest: Decimal,
    pub volume_24h: Decimal,
    pub premium: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderbookSnapshot {
    pub asset: String,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
    pub timestamp: DateTime<Utc>,
}

impl OrderbookSnapshot {
    pub fn mid_price(&self) -> Option<Decimal> {
        let best_bid = self.bids.first().map(|l| l.price)?;
        let best_ask = self.asks.first().map(|l| l.price)?;
        Some((best_bid + best_ask) / Decimal::TWO)
    }

    pub fn spread(&self) -> Option<Decimal> {
        let best_bid = self.bids.first().map(|l| l.price)?;
        let best_ask = self.asks.first().map(|l| l.price)?;
        Some(best_ask - best_bid)
    }

    pub fn spread_bps(&self) -> Option<Decimal> {
        let mid = self.mid_price()?;
        let spread = self.spread()?;
        if mid.is_zero() {
            return None;
        }
        Some(spread / mid * Decimal::new(10000, 0))
    }

    pub fn weighted_mid(&self, depth: usize) -> Option<Decimal> {
        let bids: Vec<_> = self.bids.iter().take(depth).collect();
        let asks: Vec<_> = self.asks.iter().take(depth).collect();
        if bids.is_empty() || asks.is_empty() {
            return None;
        }
        let bid_vol: Decimal = bids.iter().map(|l| l.size).sum();
        let ask_vol: Decimal = asks.iter().map(|l| l.size).sum();
        let total = bid_vol + ask_vol;
        if total.is_zero() {
            return None;
        }
        let bid_px = bids.first()?.price;
        let ask_px = asks.first()?.price;
        // Weighted mid: ask_vol pushes toward bid, bid_vol pushes toward ask
        Some((bid_px * ask_vol + ask_px * bid_vol) / total)
    }

    pub fn bid_depth(&self) -> Decimal {
        self.bids.iter().map(|l| l.size * l.price).sum()
    }

    pub fn ask_depth(&self) -> Decimal {
        self.asks.iter().map(|l| l.size * l.price).sum()
    }

    pub fn imbalance(&self) -> Decimal {
        let bd = self.bid_depth();
        let ad = self.ask_depth();
        let total = bd + ad;
        if total.is_zero() {
            return Decimal::ZERO;
        }
        (bd - ad) / total
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: Decimal,
    pub size: Decimal,
}

// ─── Candle ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candle {
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub timestamp: DateTime<Utc>,
    pub num_trades: u64,
}

// ─── Funding Rate ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FundingRate {
    pub asset: String,
    pub rate: Decimal,
    pub annualized: Decimal,
    pub timestamp: DateTime<Utc>,
}

impl FundingRate {
    pub fn new(asset: &str, rate: Decimal) -> Self {
        let annualized = rate * Decimal::new(24 * 365, 0);
        Self {
            asset: asset.to_string(),
            rate,
            annualized,
            timestamp: Utc::now(),
        }
    }
}

// ─── Alert Types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlertType {
    TradeExecuted {
        asset: String,
        side: Side,
        size: Decimal,
        price: Decimal,
        strategy: String,
    },
    RiskBreach {
        breaker: String,
        details: String,
    },
    FundingOpportunity {
        asset: String,
        rate: Decimal,
        annualized: Decimal,
    },
    DailySummary {
        nav: Decimal,
        daily_pnl: Decimal,
        positions: usize,
        active_strategies: usize,
    },
    CircuitBreakerTripped {
        breaker: String,
        reason: String,
    },
    PositionClosed {
        asset: String,
        realized_pnl: Decimal,
        reason: String,
    },
}

impl AlertType {
    pub fn format_telegram(&self) -> String {
        match self {
            AlertType::TradeExecuted {
                asset,
                side,
                size,
                price,
                strategy,
            } => {
                let emoji = match side {
                    Side::Long => "🟢",
                    Side::Short => "🔴",
                };
                format!(
                    "{emoji} <b>Trade Executed</b>\n\
                     Asset: {asset}\n\
                     Side: {side}\n\
                     Size: {size}\n\
                     Price: {price}\n\
                     Strategy: {strategy}"
                )
            }
            AlertType::RiskBreach { breaker, details } => {
                format!(
                    "⚠️ <b>Risk Breach</b>\n\
                     Breaker: {breaker}\n\
                     {details}"
                )
            }
            AlertType::FundingOpportunity {
                asset,
                rate,
                annualized,
            } => {
                format!(
                    "💰 <b>Funding Opportunity</b>\n\
                     Asset: {asset}\n\
                     Hourly: {rate}%\n\
                     Annualized: {annualized}%"
                )
            }
            AlertType::DailySummary {
                nav,
                daily_pnl,
                positions,
                active_strategies,
            } => {
                let pnl_emoji = if *daily_pnl >= Decimal::ZERO {
                    "📈"
                } else {
                    "📉"
                };
                format!(
                    "📊 <b>Daily Summary</b>\n\
                     NAV: ${nav}\n\
                     {pnl_emoji} Daily P&L: ${daily_pnl}\n\
                     Positions: {positions}\n\
                     Active Strategies: {active_strategies}"
                )
            }
            AlertType::CircuitBreakerTripped { breaker, reason } => {
                format!(
                    "🚨 <b>CIRCUIT BREAKER</b>\n\
                     Breaker: {breaker}\n\
                     Reason: {reason}"
                )
            }
            AlertType::PositionClosed {
                asset,
                realized_pnl,
                reason,
            } => {
                let emoji = if *realized_pnl >= Decimal::ZERO {
                    "✅"
                } else {
                    "❌"
                };
                format!(
                    "{emoji} <b>Position Closed</b>\n\
                     Asset: {asset}\n\
                     Realized P&L: ${realized_pnl}\n\
                     Reason: {reason}"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_side_opposite() {
        assert_eq!(Side::Long.opposite(), Side::Short);
        assert_eq!(Side::Short.opposite(), Side::Long);
    }

    #[test]
    fn test_side_sign() {
        assert_eq!(Side::Long.sign(), Decimal::ONE);
        assert_eq!(Side::Short.sign(), Decimal::NEGATIVE_ONE);
    }

    #[test]
    fn test_position_notional() {
        let pos = Position {
            asset: "BTC".to_string(),
            side: Side::Long,
            size: dec!(0.5),
            entry_price: dec!(50000),
            mark_price: dec!(51000),
            liquidation_price: None,
            unrealized_pnl: dec!(500),
            realized_pnl: Decimal::ZERO,
            leverage: dec!(2),
            margin_used: dec!(12500),
            strategy: "test".to_string(),
            opened_at: Utc::now(),
            updated_at: Utc::now(),
            cumulative_funding: Decimal::ZERO,
        };
        assert_eq!(pos.notional_value(), dec!(25500));
    }

    #[test]
    fn test_position_update_mark() {
        let mut pos = Position {
            asset: "ETH".to_string(),
            side: Side::Long,
            size: dec!(10),
            entry_price: dec!(3000),
            mark_price: dec!(3000),
            liquidation_price: None,
            unrealized_pnl: Decimal::ZERO,
            realized_pnl: Decimal::ZERO,
            leverage: dec!(3),
            margin_used: dec!(10000),
            strategy: "test".to_string(),
            opened_at: Utc::now(),
            updated_at: Utc::now(),
            cumulative_funding: Decimal::ZERO,
        };
        pos.update_mark_price(dec!(3100));
        assert_eq!(pos.unrealized_pnl, dec!(1000));

        pos.side = Side::Short;
        pos.update_mark_price(dec!(3100));
        assert_eq!(pos.unrealized_pnl, dec!(-1000));
    }

    #[test]
    fn test_portfolio_daily_return() {
        let mut port = Portfolio::new(dec!(10000));
        port.nav = dec!(10500);
        assert_eq!(port.daily_return_pct(), dec!(5));
    }

    #[test]
    fn test_portfolio_drawdown() {
        let mut port = Portfolio::new(dec!(10000));
        port.high_water_mark = dec!(12000);
        port.nav = dec!(10800);
        // (12000 - 10800) / 12000 * 100 = 10%
        assert_eq!(port.drawdown_from_hwm(), dec!(10));
    }

    #[test]
    fn test_orderbook_mid_price() {
        let ob = OrderbookSnapshot {
            asset: "BTC".to_string(),
            bids: vec![PriceLevel {
                price: dec!(50000),
                size: dec!(1),
            }],
            asks: vec![PriceLevel {
                price: dec!(50010),
                size: dec!(1),
            }],
            timestamp: Utc::now(),
        };
        assert_eq!(ob.mid_price(), Some(dec!(50005)));
        assert_eq!(ob.spread(), Some(dec!(10)));
    }

    #[test]
    fn test_orderbook_imbalance() {
        let ob = OrderbookSnapshot {
            asset: "ETH".to_string(),
            bids: vec![PriceLevel {
                price: dec!(3000),
                size: dec!(100),
            }],
            asks: vec![PriceLevel {
                price: dec!(3001),
                size: dec!(50),
            }],
            timestamp: Utc::now(),
        };
        let imb = ob.imbalance();
        assert!(imb > Decimal::ZERO); // more bid depth
    }

    #[test]
    fn test_signal_creation() {
        let sig = Signal::new(
            "BTC",
            Side::Long,
            dec!(0.8),
            dec!(0.05),
            StrategyType::FundingArb,
            "High funding rate",
        );
        assert_eq!(sig.asset, "BTC");
        assert_eq!(sig.side, Side::Long);
        assert!(!sig.is_expired());
    }

    #[test]
    fn test_funding_rate_annualized() {
        let fr = FundingRate::new("BTC", dec!(0.01));
        // 0.01 * 24 * 365 = 87.6
        assert_eq!(fr.annualized, dec!(87.60));
    }

    #[test]
    fn test_order_creation() {
        let order = Order::new_market("ETH", Side::Long, dec!(5), "momentum");
        assert_eq!(order.asset, "ETH");
        assert_eq!(order.order_type, OrderType::Market);
        assert_eq!(order.remaining_size(), dec!(5));
        assert!(!order.is_complete());
    }

    #[test]
    fn test_strategy_risk_params_default() {
        let params = StrategyRiskParams::default();
        assert_eq!(params.max_position_pct, dec!(15));
        assert_eq!(params.max_leverage, dec!(5));
    }

    #[test]
    fn test_alert_formatting() {
        let alert = AlertType::TradeExecuted {
            asset: "BTC".to_string(),
            side: Side::Long,
            size: dec!(0.1),
            price: dec!(50000),
            strategy: "momentum".to_string(),
        };
        let text = alert.format_telegram();
        assert!(text.contains("Trade Executed"));
        assert!(text.contains("BTC"));
    }
}
