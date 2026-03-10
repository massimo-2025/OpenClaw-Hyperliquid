use anyhow::Result;
use clap::Parser;
use hl_core::config::AppConfig;
use hl_core::{MarketContext, Portfolio};
use hl_data::{CandleManager, FundingMonitor, InfoClient, OrderbookManager, WebSocketManager};
use hl_execution::{OrderManager, PaperTrader, PositionTracker};
use hl_infra::dashboard::Dashboard;
use hl_infra::telegram::TelegramAlerter;
use hl_risk::circuit_breaker::CircuitBreakerEngine;
use hl_risk::PortfolioRiskManager;
use hl_strategy::StrategyRegistry;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{error, info, warn};

/// Hyperliquid Perpetual Futures Trading System
#[derive(Parser, Debug)]
#[command(name = "hyperliquid-trader")]
#[command(about = "Institutional-grade Hyperliquid perpetual futures trading bot")]
#[command(version)]
struct Cli {
    /// Single scan then exit
    #[arg(long)]
    scan_once: bool,

    /// Paper trading mode (no real orders)
    #[arg(long)]
    paper_trade: bool,

    /// Live trading mode (requires confirmation)
    #[arg(long)]
    live: bool,

    /// Scan interval in seconds
    #[arg(long, default_value = "10")]
    interval: u64,

    /// Comma-separated strategy list
    #[arg(long, default_value = "all")]
    strategies: String,

    /// Show current portfolio and exit
    #[arg(long)]
    show_portfolio: bool,

    /// Show current funding rates and exit
    #[arg(long)]
    show_funding: bool,

    /// Max portfolio leverage
    #[arg(long, default_value = "3")]
    max_leverage: f64,

    /// Paper trading starting balance
    #[arg(long, default_value = "1000")]
    starting_balance: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("hyperliquid_trader=info".parse()?)
                .add_directive("hl_core=info".parse()?)
                .add_directive("hl_data=info".parse()?)
                .add_directive("hl_strategy=info".parse()?)
                .add_directive("hl_execution=info".parse()?)
                .add_directive("hl_risk=info".parse()?)
                .add_directive("hl_infra=info".parse()?),
        )
        .init();

    let cli = Cli::parse();

    // 1. Load config from .env
    dotenvy::dotenv().ok();
    let mut config = AppConfig::from_env()?;

    // Override with CLI args
    config.trading.scan_interval_secs = cli.interval;
    config.trading.paper_trade = cli.paper_trade || !cli.live;
    config.trading.starting_balance =
        Decimal::from_f64_retain(cli.starting_balance).unwrap_or(dec!(1000));
    config.risk.max_portfolio_leverage =
        Decimal::from_f64_retain(cli.max_leverage).unwrap_or(dec!(3));
    config.trading.enabled_strategies = cli
        .strategies
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    info!("╔══════════════════════════════════════════════════════╗");
    info!("║    HYPERLIQUID TRADING SYSTEM v0.1.0                ║");
    info!("╠══════════════════════════════════════════════════════╣");
    info!(
        "║  Mode: {:<47}║",
        if config.trading.paper_trade {
            "PAPER TRADING"
        } else {
            "⚠️  LIVE TRADING"
        }
    );
    info!(
        "║  Balance: ${:<42.2}║",
        config.trading.starting_balance
    );
    info!(
        "║  Max Leverage: {:<39.1}x ║",
        config.risk.max_portfolio_leverage
    );
    info!(
        "║  Interval: {:<43}s ║",
        config.trading.scan_interval_secs
    );
    info!("╚══════════════════════════════════════════════════════╝");

    // 2. Initialize data feeds
    let info_client = InfoClient::new(&config.hyperliquid.info_url);
    let funding_monitor = FundingMonitor::new();
    let orderbook_mgr = OrderbookManager::new();

    // 3. Initialize strategies
    let mut strategy_registry =
        StrategyRegistry::create_filtered(&config.trading.enabled_strategies);
    info!(
        "Loaded {} strategies: {:?}",
        strategy_registry.count(),
        config.trading.enabled_strategies
    );

    // 4. Initialize risk engine
    let risk_manager = PortfolioRiskManager::from_config(&config.risk);
    let mut circuit_breakers = CircuitBreakerEngine::new();

    // 5. Initialize execution engine
    let mut paper_trader = PaperTrader::new(config.trading.starting_balance);
    let order_manager = OrderManager::new();

    // Try to load previous paper trading state
    if config.trading.paper_trade {
        match paper_trader.load_state().await {
            Ok(true) => info!("Loaded previous paper trading state"),
            Ok(false) => info!("Starting fresh paper trading session"),
            Err(e) => warn!("Failed to load paper state: {}", e),
        }
    }

    // 6. Initialize Telegram alerter
    let alerter = if let Some(ref tg) = config.telegram {
        TelegramAlerter::new(&tg.bot_token, &tg.chat_id)
    } else {
        TelegramAlerter::disabled()
    };

    if alerter.is_enabled() {
        let mode = if config.trading.paper_trade {
            "Paper"
        } else {
            "LIVE"
        };
        alerter
            .send_message(&format!(
                "🚀 <b>Hyperliquid Trader Started</b>\nMode: {mode}\nBalance: ${}\nStrategies: {}",
                config.trading.starting_balance,
                config.trading.enabled_strategies.join(", ")
            ))
            .await
            .ok();
    }

    // Handle show-only modes
    if cli.show_portfolio {
        let portfolio = paper_trader.portfolio().await;
        let strategy_names: Vec<String> = strategy_registry
            .strategies()
            .iter()
            .map(|s| s.name().to_string())
            .collect();
        println!("{}", Dashboard::format_portfolio(&portfolio, &strategy_names));
        return Ok(());
    }

    if cli.show_funding {
        match info_client.get_meta().await {
            Ok(assets) => {
                let rates: Vec<_> = assets
                    .iter()
                    .filter(|a| !a.funding_rate.is_zero())
                    .map(|a| {
                        (
                            a.name.clone(),
                            a.funding_rate,
                            a.funding_rate * dec!(8760),
                        )
                    })
                    .collect();
                println!("{}", Dashboard::format_funding_rates(&rates));
            }
            Err(e) => {
                error!("Failed to fetch funding rates: {}", e);
            }
        }
        return Ok(());
    }

    // Live trading safety check
    if cli.live && !config.has_live_credentials() {
        error!("Live trading requires HYPERLIQUID_WALLET_ADDRESS and HYPERLIQUID_PRIVATE_KEY");
        std::process::exit(1);
    }

    if cli.live {
        warn!("⚠️  LIVE TRADING MODE — Real orders will be placed!");
        warn!("Press Ctrl+C within 5 seconds to abort...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        info!("Live trading confirmed. Proceeding...");
    }

    // 7. Main trading loop
    info!("Starting main trading loop (interval: {}s)", cli.interval);
    let mut scan_count: u64 = 0;

    loop {
        scan_count += 1;

        // Check circuit breakers
        circuit_breakers.check_cooldowns();
        if circuit_breakers.any_tripped() {
            let tripped: Vec<_> = circuit_breakers
                .tripped_breakers()
                .iter()
                .map(|b| b.name.clone())
                .collect();
            warn!("Circuit breakers active: {:?} — skipping scan", tripped);
            tokio::time::sleep(std::time::Duration::from_secs(cli.interval)).await;
            continue;
        }

        // Build market context
        let portfolio = paper_trader.portfolio().await;

        // Try to fetch market data (graceful degradation)
        let (prices, funding_rates, assets) = match info_client.get_meta().await {
            Ok(assets) => {
                let prices: HashMap<String, Decimal> = assets
                    .iter()
                    .map(|a| (a.name.clone(), a.mark_price))
                    .collect();
                let rates: HashMap<String, Decimal> = assets
                    .iter()
                    .map(|a| (a.name.clone(), a.funding_rate))
                    .collect();
                (prices, rates, assets)
            }
            Err(e) => {
                warn!("Failed to fetch market data (scan {}): {}", scan_count, e);
                if scan_count <= 1 {
                    // First scan failure — use empty data
                    (HashMap::new(), HashMap::new(), Vec::new())
                } else {
                    tokio::time::sleep(std::time::Duration::from_secs(cli.interval)).await;
                    continue;
                }
            }
        };

        // Update paper trader prices
        paper_trader.update_prices(prices.clone());
        paper_trader.update_mark_prices().await;

        // Update funding monitor
        funding_monitor.update_rates(&funding_rates);

        let ctx = MarketContext {
            assets,
            prices: prices.clone(),
            funding_rates,
            orderbooks: HashMap::new(),
            portfolio: portfolio.clone(),
            timestamp: chrono::Utc::now(),
        };

        // Run circuit breaker evaluation
        circuit_breakers.evaluate(&portfolio);
        if circuit_breakers.any_tripped() {
            warn!("Circuit breaker tripped during evaluation");
            continue;
        }

        // Evaluate all strategies
        let mut all_signals = Vec::new();
        for strategy in strategy_registry.strategies_mut() {
            match strategy.evaluate(&ctx).await {
                Ok(signals) => {
                    for signal in signals {
                        // Risk check each signal
                        let params = strategy.risk_params();
                        match risk_manager.evaluate_signal(&signal, &portfolio, &params) {
                            Ok(approved_size) => {
                                let mut sized_signal = signal.clone();
                                sized_signal.suggested_size = approved_size;

                                // Compute units from USD size
                                if let Some(&price) = prices.get(&signal.asset) {
                                    if !price.is_zero() {
                                        sized_signal.suggested_size = approved_size / price;
                                    }
                                }

                                all_signals.push(sized_signal);
                            }
                            Err(reason) => {
                                info!(
                                    "Signal rejected: {} {} — {}",
                                    signal.side, signal.asset, reason
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Strategy {} error: {}", strategy.name(), e);
                }
            }
        }

        // Execute approved signals
        if !all_signals.is_empty() {
            info!(
                "Scan {}: {} signals approved for execution",
                scan_count,
                all_signals.len()
            );

            for signal in &all_signals {
                if config.trading.paper_trade {
                    match paper_trader.execute_signal(signal).await {
                        Ok(Some(fill)) => {
                            // Notify strategies of fills
                            for strategy in strategy_registry.strategies_mut() {
                                if format!("{}", signal.strategy) == strategy.name()
                                    || format!("{:?}", signal.strategy).contains(strategy.name())
                                {
                                    strategy.on_fill(&fill).await.ok();
                                }
                            }

                            // Send alert
                            let alert = hl_core::AlertType::TradeExecuted {
                                asset: fill.asset.clone(),
                                side: fill.side,
                                size: fill.size,
                                price: fill.price,
                                strategy: fill.strategy.clone(),
                            };
                            alerter.send_alert(&alert).await.ok();
                        }
                        Ok(None) => {}
                        Err(e) => error!("Paper trade error: {}", e),
                    }
                }
            }

            // Save paper trading state
            if config.trading.paper_trade {
                paper_trader.save_state().await.ok();
            }
        }

        // Periodic status output
        if scan_count % 6 == 0 {
            // Every ~60 seconds at 10s interval
            let portfolio = paper_trader.portfolio().await;
            info!("{}", Dashboard::format_status_line(&portfolio));
        }

        // Exit if scan-once mode
        if cli.scan_once {
            let portfolio = paper_trader.portfolio().await;
            let strategy_names: Vec<String> = strategy_registry
                .strategies()
                .iter()
                .map(|s| s.name().to_string())
                .collect();
            println!("{}", Dashboard::format_portfolio(&portfolio, &strategy_names));
            break;
        }

        tokio::time::sleep(std::time::Duration::from_secs(cli.interval)).await;
    }

    info!("Trading system shutting down");
    Ok(())
}
