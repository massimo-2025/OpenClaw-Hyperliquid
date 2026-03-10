use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use hl_core::AssetInfo;
use hl_data::InfoClient;
use hl_infra::telegram::TelegramAlerter;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use tracing::{error, info, warn};

// ─── CLI ────────────────────────────────────────────────────────────

/// Hyperliquid Perpetual Futures Scanner + Paper Trader
#[derive(Parser, Debug)]
#[command(name = "hyperliquid-trader")]
#[command(about = "Hyperliquid funding rate scanner, cross-exchange arb detector, and paper trader")]
#[command(version)]
struct Cli {
    /// Single scan then exit
    #[arg(long)]
    scan_once: bool,

    /// Paper trading mode (auto-enter best funding rate positions)
    #[arg(long)]
    paper_trade: bool,

    /// Scan interval in seconds
    #[arg(long, default_value = "30")]
    interval: u64,

    /// Show paper portfolio and exit
    #[arg(long)]
    show_portfolio: bool,

    /// Show current funding rates for all perps and exit
    #[arg(long)]
    show_funding: bool,

    /// Minimum annualized funding rate (%) to flag as opportunity
    #[arg(long, default_value = "20")]
    min_funding: f64,

    /// Paper trading starting balance (USD)
    #[arg(long, default_value = "1000")]
    starting_balance: f64,
}

// ─── Opportunity types ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct FundingOpp {
    asset: String,
    hourly_rate: f64,
    annualized_pct: f64,
    open_interest_usd: f64,
    direction: String,
}

#[derive(Debug, Clone, Serialize)]
struct CrossExchangeArb {
    asset: String,
    hl_hourly: f64,
    gio_hourly: f64,
    diff_annualized: f64,
}

#[derive(Debug, Clone, Serialize)]
struct BasisOpp {
    asset: String,
    mark_price: f64,
    oracle_price: f64,
    basis_pct: f64,
    direction: String,
}

#[derive(Debug, Clone, Serialize)]
struct SpreadOpp {
    asset: String,
    best_bid: f64,
    best_ask: f64,
    spread_bps: f64,
    mid_price: f64,
}

#[derive(Debug, Clone, Serialize)]
struct ScanResult {
    timestamp: String,
    perps_scanned: usize,
    funding_opps: Vec<FundingOpp>,
    cross_exchange_arbs: Vec<CrossExchangeArb>,
    basis_opps: Vec<BasisOpp>,
    spread_opps: Vec<SpreadOpp>,
}

// ─── Paper portfolio persistence ────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaperPortfolio {
    balance: f64,
    positions: Vec<PaperPosition>,
    total_funding_collected: f64,
    total_trades: u64,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaperPosition {
    asset: String,
    side: String,
    size_usd: f64,
    entry_price: f64,
    current_price: f64,
    hourly_funding_rate: f64,
    funding_collected: f64,
    opened_at: String,
    last_funding_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClosedPosition {
    asset: String,
    side: String,
    size_usd: f64,
    entry_price: f64,
    exit_price: f64,
    pnl_pct: f64,
    realized_pnl: f64,
    funding_collected: f64,
    hold_duration_hrs: f64,
    reason: String,
}

impl PaperPortfolio {
    fn new(balance: f64) -> Self {
        Self {
            balance,
            positions: Vec::new(),
            total_funding_collected: 0.0,
            total_trades: 0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        }
    }

    fn nav(&self) -> f64 {
        let unrealized: f64 = self.positions.iter().map(|p| {
            let direction = if p.side == "SHORT" { -1.0 } else { 1.0 };
            direction * p.size_usd * (p.current_price - p.entry_price) / p.entry_price
        }).sum();
        self.balance + unrealized + self.total_funding_collected
    }

    async fn load(path: &str) -> Option<Self> {
        let data = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str(&data).ok()
    }

    async fn save(&self, path: &str) -> Result<()> {
        let dir = Path::new(path).parent().unwrap();
        tokio::fs::create_dir_all(dir).await?;
        let json = serde_json::to_string_pretty(self)?;
        tokio::fs::write(path, json).await?;
        Ok(())
    }

    fn apply_funding(&mut self, funding_rates: &HashMap<String, f64>) {
        for pos in &mut self.positions {
            if let Some(&rate) = funding_rates.get(&pos.asset) {
                // Funding payment: rate × position_size
                // Positive rate: shorts collect from longs
                // Negative rate: longs collect from shorts
                let payment = if pos.side == "SHORT" {
                    rate * pos.size_usd  // short collects when rate positive
                } else {
                    -rate * pos.size_usd // long pays when rate positive
                };
                pos.funding_collected += payment;
                pos.hourly_funding_rate = rate;
                pos.last_funding_at = Utc::now().to_rfc3339();
                self.total_funding_collected += payment;
            }
        }
    }

    fn enter_position(&mut self, asset: &str, side: &str, size_usd: f64, price: f64, funding_rate: f64) {
        // Check if we already have a position in this asset
        if self.positions.iter().any(|p| p.asset == asset) {
            return;
        }
        // Don't exceed 20% of balance per position
        let max_size = self.balance * 0.20;
        let actual_size = size_usd.min(max_size);
        if actual_size < 10.0 {
            return;
        }

        self.positions.push(PaperPosition {
            asset: asset.to_string(),
            side: side.to_string(),
            size_usd: actual_size,
            entry_price: price,
            current_price: price,
            hourly_funding_rate: funding_rate,
            funding_collected: 0.0,
            opened_at: Utc::now().to_rfc3339(),
            last_funding_at: Utc::now().to_rfc3339(),
        });
        self.total_trades += 1;
    }

    fn update_prices(&mut self, prices: &HashMap<String, f64>) {
        for pos in &mut self.positions {
            if let Some(&price) = prices.get(&pos.asset) {
                pos.current_price = price;
            }
        }
    }

    fn close_positions_with_rules(&mut self, funding_rates: &HashMap<String, f64>, min_annualized: f64) -> Vec<ClosedPosition> {
        let min_hourly = min_annualized / (24.0 * 365.0 * 100.0);
        let now = Utc::now();
        let mut closed: Vec<ClosedPosition> = Vec::new();

        let old_positions = std::mem::take(&mut self.positions);
        for pos in old_positions {
            // Calculate PnL %
            let pnl_pct = if pos.side == "SHORT" {
                (pos.entry_price - pos.current_price) / pos.entry_price * 100.0
            } else {
                (pos.current_price - pos.entry_price) / pos.entry_price * 100.0
            };

            // Calculate hold duration
            let opened = chrono::DateTime::parse_from_rfc3339(&pos.opened_at)
                .map(|t| now.signed_duration_since(t))
                .unwrap_or_default();
            let hold_days = opened.num_hours() as f64 / 24.0;

            // Get current funding rate
            let current_rate = funding_rates.get(&pos.asset).copied().unwrap_or(0.0);
            let profitable_side = if current_rate > 0.0 { "SHORT" } else { "LONG" };
            let annualized = current_rate.abs() * 24.0 * 365.0 * 100.0;

            // ── EXIT RULES ──────────────────────────────────────
            let close_reason = if pnl_pct <= -5.0 {
                // Rule 1: STOP-LOSS at -5%
                Some(format!("🛑 STOP-LOSS: {:.1}% loss", pnl_pct))
            } else if pnl_pct >= 7.0 {
                // Rule 2: TAKE-PROFIT at +15%
                Some(format!("🎯 TAKE-PROFIT: +{:.1}%", pnl_pct))
            } else if pos.side != profitable_side && current_rate.abs() > min_hourly * 0.3 {
                // Rule 3: FUNDING FLIP — rate changed sign against us (with min threshold to avoid noise)
                Some(format!("🔄 FUNDING FLIP: now paying {:.1}% ann", annualized))
            } else if annualized < min_annualized * 0.5 && pos.side == profitable_side {
                // Rule 4: FUNDING DECAY — rate dropped below 50% of minimum threshold
                Some(format!("📉 FUNDING DECAY: rate fell to {:.1}% ann", annualized))
            } else if hold_days > 7.0 && pnl_pct < 2.0 {
                // Rule 5: MAX HOLD TIME — 7 days without significant profit
                Some(format!("⏰ MAX HOLD: {:.1} days, only {:.1}% gain", hold_days, pnl_pct))
            } else if funding_rates.get(&pos.asset).is_none() {
                // Rule 6: DELISTED — asset no longer tracked
                Some("❌ DELISTED: asset removed".to_string())
            } else {
                None // Keep position
            };

            if let Some(reason) = close_reason {
                // Realize PnL
                let direction = if pos.side == "SHORT" { -1.0 } else { 1.0 };
                let realized_pnl = direction * pos.size_usd * (pos.current_price - pos.entry_price) / pos.entry_price;
                let total_pnl = realized_pnl + pos.funding_collected;
                self.balance += pos.size_usd + total_pnl; // Return capital + PnL

                closed.push(ClosedPosition {
                    asset: pos.asset.clone(),
                    side: pos.side.clone(),
                    size_usd: pos.size_usd,
                    entry_price: pos.entry_price,
                    exit_price: pos.current_price,
                    pnl_pct,
                    realized_pnl: total_pnl,
                    funding_collected: pos.funding_collected,
                    hold_duration_hrs: opened.num_hours() as f64,
                    reason,
                });
            } else {
                self.positions.push(pos); // Keep
            }
        }

        closed
    }
}

// ─── Scan-level Telegram throttle ───────────────────────────────────

struct ScanThrottle {
    last_sent: Option<chrono::DateTime<Utc>>,
    min_interval_secs: i64,
}

impl ScanThrottle {
    fn new(min_interval_secs: i64) -> Self {
        Self {
            last_sent: None,
            min_interval_secs,
        }
    }

    fn should_send(&mut self) -> bool {
        let now = Utc::now();
        if let Some(last) = self.last_sent {
            if (now - last).num_seconds() < self.min_interval_secs {
                return false;
            }
        }
        self.last_sent = Some(now);
        true
    }
}

// ─── Scanner ────────────────────────────────────────────────────────

/// Fetch funding rates from Gate.io futures API (not geo-restricted).
/// Gate.io funding_rate is per 8-hour period; we convert to hourly.
async fn fetch_gateio_funding() -> Result<HashMap<String, f64>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client
        .get("https://api.gateio.ws/api/v4/futures/usdt/contracts")
        .send()
        .await
        .context("Failed to fetch Gate.io contracts")?;

    let data: Vec<serde_json::Value> = resp.json().await
        .context("Failed to parse Gate.io response")?;

    let mut rates = HashMap::new();
    for item in data {
        if let (Some(name), Some(rate_str)) = (
            item.get("name").and_then(|v| v.as_str()),
            item.get("funding_rate").and_then(|v| v.as_str()),
        ) {
            // Gate.io format: "BTC_USDT" → "BTC"
            if let Some(asset) = name.strip_suffix("_USDT") {
                if let Ok(rate) = rate_str.parse::<f64>() {
                    // Gate.io rate is per 8 hours → convert to hourly
                    rates.insert(asset.to_string(), rate / 8.0);
                }
            }
        }
    }

    Ok(rates)
}

async fn fetch_l2_book(client: &reqwest::Client, coin: &str) -> Result<(f64, f64)> {
    let body = serde_json::json!({"type": "l2Book", "coin": coin});
    let resp = client
        .post("https://api.hyperliquid.xyz/info")
        .json(&body)
        .send()
        .await?;

    let data: serde_json::Value = resp.json().await?;

    let best_bid = data
        .get("levels")
        .and_then(|l| l.get(0))
        .and_then(|bids| bids.get(0))
        .and_then(|b| b.get("px"))
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    let best_ask = data
        .get("levels")
        .and_then(|l| l.get(1))
        .and_then(|asks| asks.get(0))
        .and_then(|a| a.get("px"))
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);

    Ok((best_bid, best_ask))
}

fn scan_funding_opportunities(
    assets: &[AssetInfo],
    min_annualized_pct: f64,
) -> Vec<FundingOpp> {
    let mut opps: Vec<FundingOpp> = assets
        .iter()
        .filter(|a| !a.funding_rate.is_zero() && !a.mark_price.is_zero())
        .map(|a| {
            let hourly_f64 = decimal_to_f64(a.funding_rate);
            let annualized = hourly_f64 * 24.0 * 365.0 * 100.0;
            let oi_usd = decimal_to_f64(a.open_interest) * decimal_to_f64(a.mark_price);
            let direction = if hourly_f64 > 0.0 {
                "SHORT wins".to_string()
            } else {
                "LONG wins".to_string()
            };
            FundingOpp {
                asset: a.name.clone(),
                hourly_rate: hourly_f64 * 100.0,
                annualized_pct: annualized,
                open_interest_usd: oi_usd,
                direction,
            }
        })
        .filter(|o| o.annualized_pct.abs() > min_annualized_pct)
        .collect();

    opps.sort_by(|a, b| b.annualized_pct.abs().partial_cmp(&a.annualized_pct.abs()).unwrap());
    opps.truncate(20);
    opps
}

fn scan_cross_exchange_arb(
    assets: &[AssetInfo],
    other_rates: &HashMap<String, f64>,
    min_diff_annualized: f64,
) -> Vec<CrossExchangeArb> {
    let mut arbs: Vec<CrossExchangeArb> = assets
        .iter()
        .filter_map(|a| {
            let gio_hourly = other_rates.get(&a.name)?;
            let hl_hourly = decimal_to_f64(a.funding_rate);
            let diff = (hl_hourly - gio_hourly) * 24.0 * 365.0 * 100.0;
            if diff.abs() > min_diff_annualized {
                Some(CrossExchangeArb {
                    asset: a.name.clone(),
                    hl_hourly: hl_hourly * 100.0,
                    gio_hourly: gio_hourly * 100.0,
                    diff_annualized: diff,
                })
            } else {
                None
            }
        })
        .collect();

    arbs.sort_by(|a, b| b.diff_annualized.abs().partial_cmp(&a.diff_annualized.abs()).unwrap());
    arbs.truncate(15);
    arbs
}

fn scan_basis_opportunities(assets: &[AssetInfo], min_basis_pct: f64) -> Vec<BasisOpp> {
    let mut opps: Vec<BasisOpp> = assets
        .iter()
        .filter(|a| !a.mark_price.is_zero() && !a.oracle_price.is_zero())
        .filter_map(|a| {
            let mark = decimal_to_f64(a.mark_price);
            let oracle = decimal_to_f64(a.oracle_price);
            let basis_pct = (mark - oracle) / oracle * 100.0;
            if basis_pct.abs() > min_basis_pct {
                let direction = if basis_pct > 0.0 {
                    "Premium (short perp)".to_string()
                } else {
                    "Discount (long perp)".to_string()
                };
                Some(BasisOpp {
                    asset: a.name.clone(),
                    mark_price: mark,
                    oracle_price: oracle,
                    basis_pct,
                    direction,
                })
            } else {
                None
            }
        })
        .collect();

    opps.sort_by(|a, b| b.basis_pct.abs().partial_cmp(&a.basis_pct.abs()).unwrap());
    opps.truncate(10);
    opps
}

fn decimal_to_f64(d: Decimal) -> f64 {
    use std::str::FromStr;
    f64::from_str(&d.to_string()).unwrap_or(0.0)
}

// ─── Display formatting ─────────────────────────────────────────────

fn format_scan_output(result: &ScanResult, paper: Option<&PaperPortfolio>) -> String {
    let mut out = String::new();

    out.push_str("\n\x1b[1;36m🔷 HYPERLIQUID FUNDING RATE SCANNER\x1b[0m\n");
    out.push_str("══════════════════════════════════════════════════════════════\n");
    out.push_str(&format!(
        "📊 {} perps scanned | {}\n\n",
        result.perps_scanned, result.timestamp
    ));

    // Funding opportunities
    if !result.funding_opps.is_empty() {
        out.push_str("\x1b[1;33m🔥 TOP FUNDING OPPORTUNITIES (annualized):\x1b[0m\n");
        out.push_str("┌────────────┬────────────┬──────────────┬────────────┬─────────────┐\n");
        out.push_str("│ Asset      │ Funding/h  │ Annual %     │ OI ($M)    │ Direction   │\n");
        out.push_str("├────────────┼────────────┼──────────────┼────────────┼─────────────┤\n");
        for opp in &result.funding_opps {
            let sign = if opp.hourly_rate > 0.0 { "+" } else { "" };
            out.push_str(&format!(
                "│ {:<10} │ {}{:<9.4}% │ {}{:<11.1}% │ ${:<9.1} │ {:<11} │\n",
                opp.asset,
                sign,
                opp.hourly_rate,
                sign,
                opp.annualized_pct,
                opp.open_interest_usd / 1_000_000.0,
                opp.direction
            ));
        }
        out.push_str("└────────────┴────────────┴──────────────┴────────────┴─────────────┘\n\n");
    } else {
        out.push_str("📊 No significant funding opportunities (all below threshold)\n\n");
    }

    // Cross-exchange arb
    if !result.cross_exchange_arbs.is_empty() {
        out.push_str("\x1b[1;35m⚡ CROSS-EXCHANGE ARB (HL vs Gate.io):\x1b[0m\n");
        out.push_str("┌────────────┬────────────┬────────────┬──────────────┐\n");
        out.push_str("│ Asset      │ HL Rate/h  │ GIO Rate/h │ Diff (ann)   │\n");
        out.push_str("├────────────┼────────────┼────────────┼──────────────┤\n");
        for arb in &result.cross_exchange_arbs {
            let hl_sign = if arb.hl_hourly > 0.0 { "+" } else { "" };
            let gio_sign = if arb.gio_hourly > 0.0 { "+" } else { "" };
            let diff_sign = if arb.diff_annualized > 0.0 { "+" } else { "" };
            out.push_str(&format!(
                "│ {:<10} │ {}{:<9.4}% │ {}{:<9.4}% │ {}{:<11.1}% │\n",
                arb.asset,
                hl_sign, arb.hl_hourly,
                gio_sign, arb.gio_hourly,
                diff_sign, arb.diff_annualized
            ));
        }
        out.push_str("└────────────┴────────────┴────────────┴──────────────┘\n\n");
    }

    // Basis opportunities
    if !result.basis_opps.is_empty() {
        out.push_str("\x1b[1;32m📈 BASIS OPPORTUNITIES (mark vs oracle):\x1b[0m\n");
        out.push_str("┌────────────┬──────────────┬──────────────┬──────────┬─────────────────────┐\n");
        out.push_str("│ Asset      │ Mark Price   │ Oracle Price │ Basis %  │ Direction           │\n");
        out.push_str("├────────────┼──────────────┼──────────────┼──────────┼─────────────────────┤\n");
        for opp in &result.basis_opps {
            let sign = if opp.basis_pct > 0.0 { "+" } else { "" };
            out.push_str(&format!(
                "│ {:<10} │ {:<12.4} │ {:<12.4} │ {}{:<7.3}% │ {:<19} │\n",
                opp.asset,
                opp.mark_price,
                opp.oracle_price,
                sign,
                opp.basis_pct,
                opp.direction
            ));
        }
        out.push_str("└────────────┴──────────────┴──────────────┴──────────┴─────────────────────┘\n\n");
    }

    // Spread opportunities
    if !result.spread_opps.is_empty() {
        out.push_str("\x1b[1;34m💧 SPREAD OPPORTUNITIES (wide spreads):\x1b[0m\n");
        out.push_str("┌────────────┬──────────────┬──────────────┬──────────────┐\n");
        out.push_str("│ Asset      │ Best Bid     │ Best Ask     │ Spread (bps) │\n");
        out.push_str("├────────────┼──────────────┼──────────────┼──────────────┤\n");
        for opp in &result.spread_opps {
            out.push_str(&format!(
                "│ {:<10} │ {:<12.4} │ {:<12.4} │ {:<12.1} │\n",
                opp.asset, opp.best_bid, opp.best_ask, opp.spread_bps
            ));
        }
        out.push_str("└────────────┴──────────────┴──────────────┴──────────────┘\n\n");
    }

    // Paper portfolio
    if let Some(portfolio) = paper {
        out.push_str("\x1b[1;37m📋 PAPER PORTFOLIO:\x1b[0m\n");
        out.push_str(&format!("  Balance: ${:.2}\n", portfolio.balance));
        out.push_str(&format!("  NAV: ${:.2}\n", portfolio.nav()));
        out.push_str(&format!("  Funding Collected: ${:.4}\n", portfolio.total_funding_collected));
        out.push_str(&format!("  Positions: {}\n", portfolio.positions.len()));
        out.push_str(&format!("  Total Trades: {}\n", portfolio.total_trades));
        if !portfolio.positions.is_empty() {
            out.push_str("  ┌────────────┬────────┬──────────┬────────────┬────────────┬──────────────┐\n");
            out.push_str("  │ Asset      │ Side   │ Size $   │ Entry      │ Current    │ Funding $    │\n");
            out.push_str("  ├────────────┼────────┼──────────┼────────────┼────────────┼──────────────┤\n");
            for pos in &portfolio.positions {
                out.push_str(&format!(
                    "  │ {:<10} │ {:<6} │ ${:<7.1} │ {:<10.4} │ {:<10.4} │ ${:<11.4} │\n",
                    pos.asset, pos.side, pos.size_usd, pos.entry_price, pos.current_price, pos.funding_collected
                ));
            }
            out.push_str("  └────────────┴────────┴──────────┴────────────┴────────────┴──────────────┘\n");
        }
        out.push_str("\n");
    }

    out
}

fn format_funding_table(assets: &[AssetInfo]) -> String {
    let mut out = String::new();

    let mut sorted: Vec<&AssetInfo> = assets
        .iter()
        .filter(|a| !a.funding_rate.is_zero())
        .collect();
    sorted.sort_by(|a, b| {
        b.funding_rate
            .abs()
            .partial_cmp(&a.funding_rate.abs())
            .unwrap()
    });

    out.push_str("\n\x1b[1;36m🔷 ALL FUNDING RATES\x1b[0m\n");
    out.push_str("══════════════════════════════════════════════════════════════\n");
    out.push_str(&format!("📊 {} perps | {}\n\n", sorted.len(), Utc::now().format("%Y-%m-%d %H:%M UTC")));
    out.push_str("┌────────────┬────────────┬──────────────┬────────────┬──────────────┬─────────────┐\n");
    out.push_str("│ Asset      │ Funding/h  │ Annual %     │ Mark       │ OI ($M)      │ Direction   │\n");
    out.push_str("├────────────┼────────────┼──────────────┼────────────┼──────────────┼─────────────┤\n");

    for a in &sorted {
        let hourly = decimal_to_f64(a.funding_rate) * 100.0;
        let annual = hourly * 24.0 * 365.0;
        let mark = decimal_to_f64(a.mark_price);
        let oi_m = decimal_to_f64(a.open_interest) * mark / 1_000_000.0;
        let dir = if hourly > 0.0 { "SHORT wins" } else { "LONG wins" };
        let sign = if hourly > 0.0 { "+" } else { "" };
        out.push_str(&format!(
            "│ {:<10} │ {}{:<9.4}% │ {}{:<11.1}% │ {:<10.4} │ ${:<11.1} │ {:<11} │\n",
            a.name, sign, hourly, sign, annual, mark, oi_m, dir
        ));
    }
    out.push_str("└────────────┴────────────┴──────────────┴────────────┴──────────────┴─────────────┘\n");
    out
}

fn format_portfolio_display(portfolio: &PaperPortfolio) -> String {
    let mut out = String::new();
    out.push_str("\n\x1b[1;36m📋 PAPER TRADING PORTFOLIO\x1b[0m\n");
    out.push_str("══════════════════════════════════════════════════════════════\n");
    out.push_str(&format!("  Starting Balance:    ${:.2}\n", portfolio.balance));
    out.push_str(&format!("  NAV:                 ${:.2}\n", portfolio.nav()));
    out.push_str(&format!("  Total P&L:           ${:.4}\n", portfolio.nav() - portfolio.balance));
    out.push_str(&format!("  Funding Collected:   ${:.4}\n", portfolio.total_funding_collected));
    out.push_str(&format!("  Total Trades:        {}\n", portfolio.total_trades));
    out.push_str(&format!("  Active Positions:    {}\n", portfolio.positions.len()));
    out.push_str(&format!("  Last Updated:        {}\n\n", portfolio.updated_at));

    if !portfolio.positions.is_empty() {
        out.push_str("┌────────────┬────────┬──────────┬────────────┬────────────┬──────────────┬──────────────┐\n");
        out.push_str("│ Asset      │ Side   │ Size $   │ Entry      │ Current    │ Funding $    │ Opened       │\n");
        out.push_str("├────────────┼────────┼──────────┼────────────┼────────────┼──────────────┼──────────────┤\n");
        for pos in &portfolio.positions {
            let opened = &pos.opened_at[..16]; // Trim to date+hour
            out.push_str(&format!(
                "│ {:<10} │ {:<6} │ ${:<7.1} │ {:<10.4} │ {:<10.4} │ ${:<11.4} │ {:<12} │\n",
                pos.asset, pos.side, pos.size_usd, pos.entry_price, pos.current_price,
                pos.funding_collected, opened
            ));
        }
        out.push_str("└────────────┴────────┴──────────┴────────────┴────────────┴──────────────┴──────────────┘\n");
    } else {
        out.push_str("  No active positions.\n");
    }
    out
}

// ─── Telegram formatting: single consolidated message per scan ───────

fn format_telegram_alert(paper: &PaperPortfolio, closed: &[ClosedPosition]) -> Option<String> {
    let mut alerts: Vec<String> = Vec::new();

    // Alert 1: Positions that were closed this cycle
    for c in closed {
        let pnl_str = if c.realized_pnl >= 0.0 {
            format!("+${:.2}", c.realized_pnl)
        } else {
            format!("-${:.2}", c.realized_pnl.abs())
        };
        alerts.push(format!(
            "🔒 CLOSED {} {} — {} ({}) | Held {:.0}h | Funding ${:.4}",
            c.asset, c.side, c.reason, pnl_str, c.hold_duration_hrs, c.funding_collected
        ));
    }

    // Alert 2: Open positions with >=10% move from entry
    for pos in &paper.positions {
        if pos.entry_price > 0.0 {
            let pnl_pct = if pos.side == "SHORT" {
                (pos.entry_price - pos.current_price) / pos.entry_price * 100.0
            } else {
                (pos.current_price - pos.entry_price) / pos.entry_price * 100.0
            };
            if pnl_pct.abs() >= 10.0 {
                let emoji = if pnl_pct >= 0.0 { "📈" } else { "📉" };
                alerts.push(format!(
                    "{} {} {} {:+.1}% (${:.4} → ${:.4})",
                    emoji, pos.asset, pos.side, pnl_pct, pos.entry_price, pos.current_price
                ));
            }
        }
    }

    if alerts.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str(&format!("🔷 HYPERLIQUID ALERT\n\n"));
    for alert in &alerts {
        out.push_str(&format!("{}\n", alert));
    }
    out.push_str(&format!(
        "\n💰 NAV: ${:.2} | PnL: {:+.2}% | Funding: ${:.4}",
        paper.nav(), (paper.nav() - 110.0) / 110.0 * 100.0, paper.total_funding_collected
    ));

    Some(out)
}

#[allow(dead_code)]
fn format_telegram_consolidated_old(result: &ScanResult, paper: Option<&PaperPortfolio>) -> Option<String> {
    // Legacy: full scan digest (disabled)
    let has_funding = !result.funding_opps.is_empty();
    let has_arbs = !result.cross_exchange_arbs.is_empty();
    let has_basis = !result.basis_opps.is_empty();
    let has_paper = paper.is_some();

    if !has_funding && !has_arbs && !has_basis && !has_paper {
        return None;
    }

    let mut out = String::new();

    out.push_str(&format!(
        "🔷 <b>HYPERLIQUID SCAN</b> — {}\n\n",
        result.timestamp
    ));

    // Funding section
    if has_funding {
        out.push_str(&format!(
            "🔥 <b>FUNDING ({} opportunities):</b>\n",
            result.funding_opps.len()
        ));
        for opp in result.funding_opps.iter().take(8) {
            let dir = if opp.annualized_pct > 0.0 { "SHORT collects" } else { "LONG collects" };
            out.push_str(&format!(
                "• {}: {:+.0}% ann → {}\n",
                opp.asset, opp.annualized_pct, dir
            ));
        }
        if result.funding_opps.len() > 8 {
            out.push_str(&format!("  … +{} more\n", result.funding_opps.len() - 8));
        }
        out.push('\n');
    } else {
        out.push_str("🔥 <b>FUNDING (0 opportunities)</b>\n\n");
    }

    // Cross-exchange arb section
    if has_arbs {
        out.push_str(&format!(
            "⚡ <b>CROSS-EXCHANGE ARB ({}):</b>\n",
            result.cross_exchange_arbs.len()
        ));
        for arb in result.cross_exchange_arbs.iter().take(5) {
            out.push_str(&format!(
                "• {}: HL {:+.4}% vs GIO {:+.4}% = {:+.0}% ann\n",
                arb.asset, arb.hl_hourly, arb.gio_hourly, arb.diff_annualized
            ));
        }
        out.push('\n');
    } else {
        out.push_str("⚡ <b>CROSS-EXCHANGE ARB (0)</b>\n\n");
    }

    // Basis section
    if has_basis {
        out.push_str(&format!(
            "📈 <b>BASIS ({} opportunities):</b>\n",
            result.basis_opps.len()
        ));
        for opp in result.basis_opps.iter().take(5) {
            out.push_str(&format!(
                "• {}: {:+.2}% {}\n",
                opp.asset, opp.basis_pct, opp.direction
            ));
        }
        out.push('\n');
    } else {
        out.push_str("📈 <b>BASIS (0 opportunities)</b>\n\n");
    }

    // Paper portfolio section
    if let Some(portfolio) = paper {
        let pnl = portfolio.nav() - portfolio.balance;
        let pnl_pct = if portfolio.balance > 0.0 { pnl / portfolio.balance * 100.0 } else { 0.0 };
        let emoji = if pnl >= 0.0 { "📈" } else { "📉" };
        out.push_str(&format!(
            "💰 <b>Paper PnL:</b> {} {:+.2} ({:+.1}%) | Funding: ${:.4} | Pos: {}",
            emoji, pnl, pnl_pct, portfolio.total_funding_collected, portfolio.positions.len()
        ));
    }

    Some(out)
}

// ─── Log opportunities to JSONL ─────────────────────────────────────

async fn log_opportunities(result: &ScanResult) -> Result<()> {
    let log_dir = Path::new("logs");
    tokio::fs::create_dir_all(log_dir).await?;

    let line = serde_json::to_string(result)?;
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("logs/opportunities.jsonl")
        .await?;

    use tokio::io::AsyncWriteExt;
    file.write_all(line.as_bytes()).await?;
    file.write_all(b"\n").await?;
    Ok(())
}

// ─── Main ───────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("hyperliquid_trader=info".parse()?)
                .add_directive("hl_data=warn".parse()?)
                .add_directive("hl_infra=warn".parse()?),
        )
        .init();

    let cli = Cli::parse();

    // Load .env from hyperliquid-agent
    let env_path = "/home/clawd/.openclaw/workspace/hyperliquid-agent/.env";
    if Path::new(env_path).exists() {
        dotenvy::from_path(env_path).ok();
    }
    dotenvy::dotenv().ok();

    let min_funding_pct = cli.min_funding;
    let min_arb_pct = 10.0; // min cross-exchange arb differential annualized
    let min_basis_pct = 0.5; // min basis % for opportunity
    let paper_data_path = "data/paper-portfolio.json";

    // Initialize Telegram alerter
    let bot_token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_default();
    let chat_id = std::env::var("TELEGRAM_CHAT_ID").unwrap_or_default();
    let alerter = if !bot_token.is_empty() && !chat_id.is_empty() {
        TelegramAlerter::new(&bot_token, &chat_id)
    } else {
        TelegramAlerter::disabled()
    };

    // Initialize info client
    let info_url = std::env::var("HYPERLIQUID_INFO_URL")
        .unwrap_or_else(|_| "https://api.hyperliquid.xyz/info".to_string());
    let info_client = InfoClient::new(&info_url);
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // ─── Show portfolio mode ────────────────────────────────────
    if cli.show_portfolio {
        if let Some(portfolio) = PaperPortfolio::load(paper_data_path).await {
            println!("{}", format_portfolio_display(&portfolio));
        } else {
            println!("No paper portfolio found. Start with --paper-trade to create one.");
        }
        return Ok(());
    }

    // ─── Show funding mode ──────────────────────────────────────
    if cli.show_funding {
        info!("Fetching funding rates from Hyperliquid...");
        let assets = info_client.get_meta().await?;
        println!("{}", format_funding_table(&assets));
        return Ok(());
    }

    // ─── Banner ─────────────────────────────────────────────────
    let mode = if cli.paper_trade {
        "PAPER TRADE"
    } else {
        "SCAN ONLY"
    };
    let balance = cli.starting_balance;

    println!("\n╔══════════════════════════════════════════════════════════╗");
    println!("║    HYPERLIQUID TRADING SCANNER v0.1.0                   ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  Mode:          {:<40}║", mode);
    println!("║  Min Funding:   {:<40}║", format!("{}% annualized", min_funding_pct));
    println!("║  Interval:      {:<40}║", format!("{}s", cli.interval));
    if cli.paper_trade {
        println!("║  Balance:       {:<40}║", format!("${}", balance));
    }
    println!("║  Telegram:      {:<40}║", if alerter.is_enabled() { "✅ Enabled" } else { "❌ Disabled" });
    println!("╚══════════════════════════════════════════════════════════╝\n");

    // Send startup notification
    if alerter.is_enabled() {
        alerter
            .send_message(&format!(
                "🚀 <b>Hyperliquid Scanner Started</b>\nMode: {}\nMin Funding: {}%\nInterval: {}s",
                mode, min_funding_pct, cli.interval
            ))
            .await
            .ok();
    }

    // Load or create paper portfolio
    let mut paper_portfolio = if cli.paper_trade {
        PaperPortfolio::load(paper_data_path)
            .await
            .unwrap_or_else(|| {
                info!("Creating new paper portfolio with ${} balance", balance);
                PaperPortfolio::new(balance)
            })
    } else {
        PaperPortfolio::new(balance)
    };

    // One consolidated Telegram message per scan; throttle to at most 1 per interval
    let mut scan_throttle = ScanThrottle::new(cli.interval as i64);
    let mut scan_count: u64 = 0;

    loop {
        scan_count += 1;

        // ── Fetch HL market data ────────────────────────────────
        let assets = match info_client.get_meta().await {
            Ok(a) => a,
            Err(e) => {
                warn!("Scan {}: Failed to fetch HL data: {}", scan_count, e);
                if cli.scan_once {
                    error!("Cannot complete scan-once: {}", e);
                    return Err(e);
                }
                tokio::time::sleep(std::time::Duration::from_secs(cli.interval)).await;
                continue;
            }
        };

        // ── Fetch Gate.io funding rates for cross-exchange arb ──
        let gateio_rates = match fetch_gateio_funding().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to fetch Gate.io rates: {}", e);
                HashMap::new()
            }
        };

        // ── Scan for opportunities ──────────────────────────────
        let funding_opps = scan_funding_opportunities(&assets, min_funding_pct);
        let cross_arbs = scan_cross_exchange_arb(&assets, &gateio_rates, min_arb_pct);
        let basis_opps = scan_basis_opportunities(&assets, min_basis_pct);

        // ── Fetch L2 orderbooks for top liquid coins ────────────
        let top_coins: Vec<String> = {
            let mut sorted = assets.clone();
            sorted.sort_by(|a, b| b.volume_24h.partial_cmp(&a.volume_24h).unwrap());
            sorted.iter().take(10).map(|a| a.name.clone()).collect()
        };

        let mut spread_opps: Vec<SpreadOpp> = Vec::new();
        for coin in &top_coins {
            match fetch_l2_book(&http_client, coin).await {
                Ok((bid, ask)) => {
                    if bid > 0.0 && ask > 0.0 {
                        let mid = (bid + ask) / 2.0;
                        let spread_bps = (ask - bid) / mid * 10000.0;
                        if spread_bps > 1.0 {
                            // Flag spreads > 1 bps as interesting
                            spread_opps.push(SpreadOpp {
                                asset: coin.clone(),
                                best_bid: bid,
                                best_ask: ask,
                                spread_bps,
                                mid_price: mid,
                            });
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch orderbook for {}: {}", coin, e);
                }
            }
        }
        spread_opps.sort_by(|a, b| b.spread_bps.partial_cmp(&a.spread_bps).unwrap());
        spread_opps.truncate(10);

        let result = ScanResult {
            timestamp: Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            perps_scanned: assets.len(),
            funding_opps: funding_opps.clone(),
            cross_exchange_arbs: cross_arbs.clone(),
            basis_opps: basis_opps.clone(),
            spread_opps: spread_opps.clone(),
        };

        // ── Paper trading: apply funding + enter positions ──────
        if cli.paper_trade {
            // Build price + funding maps
            let prices: HashMap<String, f64> = assets
                .iter()
                .map(|a| (a.name.clone(), decimal_to_f64(a.mark_price)))
                .collect();
            let funding: HashMap<String, f64> = assets
                .iter()
                .map(|a| (a.name.clone(), decimal_to_f64(a.funding_rate)))
                .collect();

            // Apply funding payments (proportional to scan interval vs 1 hour)
            let interval_fraction = cli.interval as f64 / 3600.0;
            let mut scaled_funding: HashMap<String, f64> = HashMap::new();
            for (k, v) in &funding {
                scaled_funding.insert(k.clone(), v * interval_fraction);
            }
            paper_portfolio.apply_funding(&scaled_funding);
            paper_portfolio.update_prices(&prices);

            // Close positions using full rule engine (stop-loss, take-profit, funding flip, decay, max hold)
            let closed_positions = paper_portfolio.close_positions_with_rules(&funding, min_funding_pct);

            // Log closed positions
            for c in &closed_positions {
                info!(
                    "📋 Closed {} {} — {} | PnL: ${:.2} ({:+.1}%) | Funding: ${:.4} | Held: {:.0}h",
                    c.asset, c.side, c.reason, c.realized_pnl, c.pnl_pct, c.funding_collected, c.hold_duration_hrs
                );
            }

            // Enter new positions on best funding opps (max 5 positions)
            if paper_portfolio.positions.len() < 5 {
                for opp in &funding_opps {
                    if paper_portfolio.positions.len() >= 5 {
                        break;
                    }
                    // Only enter if annualized > 30% and OI > $1M
                    if opp.annualized_pct.abs() > 30.0 && opp.open_interest_usd > 1_000_000.0 {
                        let side = if opp.annualized_pct > 0.0 { "SHORT" } else { "LONG" };
                        let price = prices.get(&opp.asset).copied().unwrap_or(0.0);
                        if price > 0.0 {
                            paper_portfolio.enter_position(
                                &opp.asset,
                                side,
                                50.0, // $50 per position
                                price,
                                *funding.get(&opp.asset).unwrap_or(&0.0),
                            );
                        }
                    }
                }
            }

            paper_portfolio.updated_at = Utc::now().to_rfc3339();
            paper_portfolio.save(paper_data_path).await.ok();

            // ── Telegram: only alert on closes or >=10% position moves ──
            if alerter.is_enabled() {
                if let Some(msg) = format_telegram_alert(&paper_portfolio, &closed_positions) {
                    alerter.send_message(&msg).await.ok();
                }
            }
        }

        // ── Display output ──────────────────────────────────────
        let paper_ref = if cli.paper_trade {
            Some(&paper_portfolio)
        } else {
            None
        };
        let output = format_scan_output(&result, paper_ref);
        println!("{}", output);

        // ── Log to JSONL ────────────────────────────────────────
        log_opportunities(&result).await.ok();

        // ── Exit if scan-once ───────────────────────────────────
        if cli.scan_once {
            break;
        }

        info!(
            "Scan {} complete: {} funding opps, {} arbs, {} basis, {} spreads. Next in {}s",
            scan_count,
            result.funding_opps.len(),
            result.cross_exchange_arbs.len(),
            result.basis_opps.len(),
            result.spread_opps.len(),
            cli.interval
        );

        tokio::time::sleep(std::time::Duration::from_secs(cli.interval)).await;
    }

    Ok(())
}
