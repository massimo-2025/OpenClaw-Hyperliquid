#!/usr/bin/env bash
# Portfolio dashboard — reads data/paper-portfolio.json and pretty-prints.
set -euo pipefail

STATE_FILE="${1:-data/paper-portfolio.json}"

if [ ! -f "$STATE_FILE" ]; then
    echo "No portfolio state found at $STATE_FILE"
    echo "Run the trader first: hyperliquid-trader --paper-trade --scan-once"
    exit 1
fi

echo ""
echo "╔══════════════════════════════════════════════════════╗"
echo "║       PAPER TRADING PORTFOLIO STATUS                ║"
echo "╠══════════════════════════════════════════════════════╣"

NAV=$(jq -r '.nav' "$STATE_FILE")
CASH=$(jq -r '.cash' "$STATE_FILE")
BALANCE=$(jq -r '.starting_balance' "$STATE_FILE")
PNL=$(jq -r '.total_realized_pnl' "$STATE_FILE")
HWM=$(jq -r '.high_water_mark' "$STATE_FILE")
TRADES=$(jq -r '.total_trades' "$STATE_FILE")
UPDATED=$(jq -r '.updated_at' "$STATE_FILE")

printf "║  NAV:              %-34s ║\n" "\$$NAV"
printf "║  Cash:             %-34s ║\n" "\$$CASH"
printf "║  Starting Balance: %-34s ║\n" "\$$BALANCE"
printf "║  Realized P&L:     %-34s ║\n" "\$$PNL"
printf "║  High Water Mark:  %-34s ║\n" "\$$HWM"
printf "║  Total Trades:     %-34s ║\n" "$TRADES"
printf "║  Last Updated:     %-34s ║\n" "$UPDATED"

echo "╠══════════════════════════════════════════════════════╣"

POSITIONS=$(jq -r '.positions | length' "$STATE_FILE")
if [ "$POSITIONS" -eq 0 ]; then
    echo "║  No open positions                                   ║"
else
    echo "║  POSITIONS ($POSITIONS open)                                   ║"
    echo "║  ─────────────────────────────────────────────────── ║"
    jq -r '.positions[] | "║  \(.asset)\t\(.side)\tsize=\(.size)\tentry=\(.entry_price)\tmark=\(.mark_price) ║"' "$STATE_FILE"
fi

echo "╠══════════════════════════════════════════════════════╣"
echo "║  STRATEGY P&L                                       ║"
jq -r '.strategy_pnl | to_entries[] | "║  \(.key): $\(.value)"' "$STATE_FILE" 2>/dev/null || echo "║  No strategy data                                    ║"

echo "╚══════════════════════════════════════════════════════╝"
echo ""
