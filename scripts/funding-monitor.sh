#!/usr/bin/env bash
# Funding rate monitor — fetches and displays top funding opportunities from Hyperliquid.
set -euo pipefail

API_URL="${HYPERLIQUID_INFO_URL:-https://api.hyperliquid.xyz/info}"

echo ""
echo "┌─────────────────────────────────────────────────────┐"
echo "│       HYPERLIQUID FUNDING RATE MONITOR              │"
echo "└─────────────────────────────────────────────────────┘"
echo ""

# Fetch meta and asset contexts
RESPONSE=$(curl -s -X POST "$API_URL" \
    -H "Content-Type: application/json" \
    -d '{"type": "metaAndAssetCtxs"}')

if [ -z "$RESPONSE" ]; then
    echo "Error: Failed to fetch data from $API_URL"
    exit 1
fi

echo "Top 20 Funding Rates (sorted by |annualized|):"
echo "────────────────────────────────────────────────"
printf "%-10s %-14s %-16s %-12s\n" "Asset" "Hourly" "Annualized" "OI"
echo "────────────────────────────────────────────────"

# Extract and sort by absolute funding rate
echo "$RESPONSE" | jq -r '
    .[0].universe as $meta |
    .[1] as $ctx |
    [range(0; $meta | length)] |
    map({
        name: $meta[.].name,
        funding: ($ctx[.].funding // "0"),
        oi: ($ctx[.].openInterest // "0")
    }) |
    sort_by(-.funding | tonumber | fabs) |
    .[0:20][] |
    "\(.name)\t\(.funding)\t\(.oi)"
' 2>/dev/null | while IFS=$'\t' read -r name funding oi; do
    # Calculate annualized
    annual=$(echo "$funding * 8760" | bc -l 2>/dev/null || echo "0")
    funding_pct=$(echo "$funding * 100" | bc -l 2>/dev/null || echo "0")

    # Emoji based on direction
    if (( $(echo "$funding > 0" | bc -l) )); then
        emoji="🟢"
    elif (( $(echo "$funding < 0" | bc -l) )); then
        emoji="🔴"
    else
        emoji="⚪"
    fi

    printf "%s %-8s %+.6f%%    %+.2f%%         %s\n" "$emoji" "$name" "$funding_pct" "$annual" "$oi"
done

echo ""
echo "🟢 Positive: Longs pay shorts (short to collect)"
echo "🔴 Negative: Shorts pay longs (long to collect)"
echo ""
