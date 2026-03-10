# Hyperliquid Perpetual Futures Trading System

Institutional-grade automated trading system for Hyperliquid perpetual futures. Built in Rust for maximum performance and reliability.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Main Orchestrator                        │
│                     (CLI + Strategy Loop)                        │
├──────────┬──────────┬──────────┬──────────┬──────────┬─────────┤
│   core   │   data   │ strategy │execution │   risk   │  infra  │
│          │          │          │          │          │         │
│ • Types  │ • Info   │ • Fund.  │ • Client │ • Portf. │ • Tele. │
│ • Config │ • WS     │ • Basis  │ • Orders │ • Margin │ • gRPC  │
│ • Wallet │ • Book   │ • MM     │ • Paper  │ • Kelly  │ • Metr. │
│          │ • Fund.  │ • Liq.   │ • Pos.   │ • DD     │ • Bina. │
│          │ • Candle │ • Stat   │          │ • Corr.  │ • Dash. │
│          │          │ • LLM    │          │ • CB     │         │
│          │          │ • Mom.   │          │          │         │
└──────────┴──────────┴──────────┴──────────┴──────────┴─────────┘
```

## Strategies

| # | Strategy | Description | Edge Source |
|---|----------|-------------|-------------|
| 1 | **Funding Arbitrage** | Collect funding by positioning opposite to crowded trades | Funding rate differential |
| 2 | **Basis Trade** | Spot-perp convergence when basis widens | Basis mean reversion |
| 3 | **Market Making** | Two-sided quotes with inventory management | Bid-ask spread + maker rebate |
| 4 | **Liquidation Detection** | Position before cascade liquidations | Forced selling pressure |
| 5 | **Statistical Arbitrage** | Pairs trading with Kalman filter hedge ratios | Spread mean reversion |
| 6 | **LLM Signals** | External signals via gRPC from OpenClaw | AI-driven directional alpha |
| 7 | **Momentum/Breakout** | EMA crossovers + volume + ADX confirmation | Trend following |

## Risk Framework

- **Portfolio leverage cap:** 3x maximum
- **Position sizing:** Fractional Kelly (quarter-Kelly) with leverage adjustment
- **Max single position:** 15% of NAV
- **Max correlated exposure:** 30% of NAV
- **Drawdown management:**
  - Progressive reduction starting at -5%
  - Daily kill switch at -10%
  - Total kill switch at -20%
  - 1-hour cooldown after breach

### Circuit Breakers (7 independent)

1. **DrawdownBreaker** — Daily/total loss limits
2. **MarginBreaker** — Margin < 2x liquidation buffer
3. **VolatilityBreaker** — Realized vol > 3x historical
4. **CorrelationBreaker** — Correlation spike (regime change)
5. **APIBreaker** — Repeated API errors or latency
6. **FundingBreaker** — Unexpected funding rate reversal
7. **ManualBreaker** — Telegram command override

## Setup

### Prerequisites

- Rust 1.70+ (`rustup install stable`)
- Protobuf compiler (`protoc`)

### Build

```bash
git clone https://github.com/massimo-2025/OpenClaw-Hyperliquid.git
cd OpenClaw-Hyperliquid
cp .env.example .env
# Edit .env with your credentials
cargo build --release
```

### Configuration

All configuration via environment variables (`.env` file):

```bash
# Required for live trading
HYPERLIQUID_WALLET_ADDRESS=0x...
HYPERLIQUID_PRIVATE_KEY=0x...

# Optional: Telegram alerts
TELEGRAM_BOT_TOKEN=your-bot-token
TELEGRAM_CHAT_ID=your-chat-id

# Risk parameters (defaults shown)
MAX_PORTFOLIO_LEVERAGE=3.0
MAX_POSITION_PCT=15.0
DAILY_DRAWDOWN_LIMIT=10.0
TOTAL_DRAWDOWN_LIMIT=20.0
```

## Usage

```bash
# Paper trading (default, no credentials needed)
./target/release/hyperliquid-trader --paper-trade

# Single scan and exit
./target/release/hyperliquid-trader --scan-once

# Show current portfolio
./target/release/hyperliquid-trader --show-portfolio

# Show funding rates
./target/release/hyperliquid-trader --show-funding

# Select specific strategies
./target/release/hyperliquid-trader --strategies funding_arb,momentum

# Custom parameters
./target/release/hyperliquid-trader --paper-trade --interval 30 --max-leverage 2 --starting-balance 5000

# Live trading (requires credentials + confirmation)
./target/release/hyperliquid-trader --live
```

### CLI Options

| Flag | Default | Description |
|------|---------|-------------|
| `--paper-trade` | (default) | Paper trading mode |
| `--live` | off | Live trading (real orders) |
| `--scan-once` | off | Single scan then exit |
| `--interval` | 10 | Scan interval in seconds |
| `--strategies` | all | Comma-separated strategy list |
| `--show-portfolio` | off | Show portfolio and exit |
| `--show-funding` | off | Show funding rates and exit |
| `--max-leverage` | 3 | Max portfolio leverage |
| `--starting-balance` | 1000 | Paper trading starting balance |

## Scripts

```bash
# Portfolio dashboard
./scripts/portfolio.sh

# Funding rate monitor
./scripts/funding-monitor.sh
```

## Deployment

```bash
# Install systemd service (user mode)
cp deploy/hyperliquid-trader.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable hyperliquid-trader
systemctl --user start hyperliquid-trader

# View logs
journalctl --user -u hyperliquid-trader -f
```

## Security

- **Zero hardcoded credentials** — all secrets via environment variables
- `.env` is in `.gitignore` — never committed
- Private keys never logged or transmitted
- Systemd service runs with `NoNewPrivileges` and `ProtectSystem=strict`
- Paper trading works without any credentials

## License

MIT

## Author

Massimo Mazza
