use hl_core::{Portfolio, Position};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Terminal dashboard for portfolio status and strategy breakdown.
pub struct Dashboard;

impl Dashboard {
    /// Format a complete portfolio dashboard.
    pub fn format_portfolio(portfolio: &Portfolio, strategy_names: &[String]) -> String {
        let mut out = String::new();

        out.push_str("\n╔══════════════════════════════════════════════════════════╗\n");
        out.push_str("║          HYPERLIQUID TRADING SYSTEM — DASHBOARD         ║\n");
        out.push_str("╠══════════════════════════════════════════════════════════╣\n");
        out.push_str(&format!(
            "║  NAV:              ${:<36.2} ║\n",
            portfolio.nav
        ));
        out.push_str(&format!(
            "║  Cash:             ${:<36.2} ║\n",
            portfolio.cash
        ));
        out.push_str(&format!(
            "║  Unrealized P&L:   ${:<36.2} ║\n",
            portfolio.total_unrealized_pnl
        ));
        out.push_str(&format!(
            "║  Realized P&L:     ${:<36.2} ║\n",
            portfolio.total_realized_pnl
        ));
        out.push_str(&format!(
            "║  Daily P&L:        ${:<36.2} ║\n",
            portfolio.daily_pnl()
        ));
        out.push_str(&format!(
            "║  Leverage:         {:<37.2}x ║\n",
            portfolio.leverage
        ));
        out.push_str(&format!(
            "║  Margin Ratio:     {:<37.2}x ║\n",
            portfolio.margin_ratio
        ));
        out.push_str(&format!(
            "║  HWM:              ${:<36.2} ║\n",
            portfolio.high_water_mark
        ));
        out.push_str(&format!(
            "║  Drawdown:         {:<37.2}% ║\n",
            portfolio.drawdown_from_hwm()
        ));
        out.push_str(&format!(
            "║  Positions:        {:<38} ║\n",
            portfolio.positions.len()
        ));
        out.push_str(&format!(
            "║  Strategies:       {:<38} ║\n",
            strategy_names.len()
        ));
        out.push_str("╠══════════════════════════════════════════════════════════╣\n");

        if portfolio.positions.is_empty() {
            out.push_str("║  No open positions                                       ║\n");
        } else {
            out.push_str("║  POSITIONS                                               ║\n");
            out.push_str("║  Asset    Side    Size       Entry      Mark       P&L    ║\n");
            out.push_str("║  ──────   ─────   ────────   ────────   ────────   ────── ║\n");

            for pos in &portfolio.positions {
                out.push_str(&format!(
                    "║  {:<8} {:<6} {:<10} {:<10} {:<10} {:<7.2} ║\n",
                    pos.asset,
                    format!("{}", pos.side),
                    format!("{:.4}", pos.size),
                    format!("{:.2}", pos.entry_price),
                    format!("{:.2}", pos.mark_price),
                    pos.total_pnl()
                ));
            }
        }

        out.push_str("╚══════════════════════════════════════════════════════════╝\n");
        out
    }

    /// Format a compact one-line status.
    pub fn format_status_line(portfolio: &Portfolio) -> String {
        let pnl_emoji = if portfolio.daily_pnl() >= Decimal::ZERO {
            "📈"
        } else {
            "📉"
        };

        format!(
            "{pnl_emoji} NAV: ${:.2} | P&L: ${:.2} | Lev: {:.2}x | Pos: {} | DD: {:.2}%",
            portfolio.nav,
            portfolio.daily_pnl(),
            portfolio.leverage,
            portfolio.positions.len(),
            portfolio.drawdown_from_hwm()
        )
    }

    /// Format funding rate opportunities.
    pub fn format_funding_rates(rates: &[(String, Decimal, Decimal)]) -> String {
        let mut out = String::new();
        out.push_str("\n┌─────────────────────────────────────────┐\n");
        out.push_str("│    TOP FUNDING RATE OPPORTUNITIES       │\n");
        out.push_str("├──────────┬────────────┬─────────────────┤\n");
        out.push_str("│ Asset    │ Hourly     │ Annualized      │\n");
        out.push_str("├──────────┼────────────┼─────────────────┤\n");

        for (asset, hourly, annualized) in rates {
            let emoji = if *annualized > Decimal::ZERO {
                "🟢"
            } else {
                "🔴"
            };
            out.push_str(&format!(
                "│ {emoji} {:<6} │ {:<10.6}%│ {:<15.2}% │\n",
                asset,
                hourly * dec!(100),
                annualized
            ));
        }

        out.push_str("└──────────┴────────────┴─────────────────┘\n");
        out
    }

    /// Format risk metrics.
    pub fn format_risk_metrics(
        leverage: Decimal,
        margin_ratio: Decimal,
        drawdown: Decimal,
        circuit_breakers_active: usize,
    ) -> String {
        let lev_status = if leverage > dec!(2.5) {
            "🔴"
        } else if leverage > dec!(1.5) {
            "🟡"
        } else {
            "🟢"
        };

        let margin_status = if margin_ratio < dec!(2) {
            "🔴"
        } else if margin_ratio < dec!(3) {
            "🟡"
        } else {
            "🟢"
        };

        let dd_status = if drawdown > dec!(10) {
            "🔴"
        } else if drawdown > dec!(5) {
            "🟡"
        } else {
            "🟢"
        };

        format!(
            "Risk: {lev_status} Lev {:.2}x | {margin_status} Margin {:.2}x | {dd_status} DD {:.2}% | CB: {}",
            leverage, margin_ratio, drawdown, circuit_breakers_active
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_portfolio() {
        let portfolio = Portfolio::new(dec!(10000));
        let strategies = vec!["FundingArb".to_string(), "Momentum".to_string()];
        let output = Dashboard::format_portfolio(&portfolio, &strategies);
        assert!(output.contains("DASHBOARD"));
        assert!(output.contains("10000"));
    }

    #[test]
    fn test_format_status_line() {
        let portfolio = Portfolio::new(dec!(10000));
        let line = Dashboard::format_status_line(&portfolio);
        assert!(line.contains("NAV"));
        assert!(line.contains("10000"));
    }

    #[test]
    fn test_format_funding_rates() {
        let rates = vec![
            ("BTC".to_string(), dec!(0.0003), dec!(2.628)),
            ("DOGE".to_string(), dec!(0.005), dec!(43.8)),
        ];
        let output = Dashboard::format_funding_rates(&rates);
        assert!(output.contains("BTC"));
        assert!(output.contains("DOGE"));
    }

    #[test]
    fn test_format_risk_metrics() {
        let output = Dashboard::format_risk_metrics(
            dec!(1.5),
            dec!(3.5),
            dec!(3),
            0,
        );
        assert!(output.contains("Lev"));
        assert!(output.contains("Margin"));
        assert!(output.contains("DD"));
    }
}
