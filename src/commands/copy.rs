use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{Hash, Hasher},
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use clap::{Args, Subcommand, ValueEnum};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::output::OutputFormat;
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::clob::types::request::OrderBookSummaryRequest;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side as ClobSide};
use polymarket_client_sdk::data::types::request::{
    ClosedPositionsRequest, TradesRequest, ValueRequest,
};

#[derive(Args)]
pub struct CopyArgs {
    #[command(subcommand)]
    pub command: CopyCommand,
}

#[derive(Subcommand)]
pub enum CopyCommand {
    Configure(ConfigureArgs),
    Status,
    Plan(PlanArgs),
    Record(RecordArgs),
    Settle(SettleArgs),
    Dashboard,
    /// Local web UI with near-real-time updates and controls
    Ui(UiArgs),
}

#[derive(Args)]
pub struct UiArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 8787)]
    pub port: u16,
}

#[derive(Args, Serialize, Deserialize)]
pub struct ConfigureArgs {
    #[arg(long)]
    pub leader: String,
    #[arg(long)]
    pub allocated_funds: Decimal,
    #[arg(long, default_value_t = Decimal::from_i128_with_scale(500, 2))]
    pub max_trade_pct: Decimal,
    #[arg(long, default_value_t = Decimal::from_i128_with_scale(7000, 2))]
    pub max_total_exposure_pct: Decimal,
    #[arg(long, default_value_t = Decimal::ONE)]
    pub min_copy_usd: Decimal,
    #[arg(long, default_value_t = 2)]
    pub poll_interval_secs: u64,
    /// Optional polling interval in milliseconds (min 500ms). Overrides poll-interval-secs when set.
    #[arg(long)]
    pub poll_interval_ms: Option<u64>,
    #[arg(long, value_enum, default_value_t = RiskLevel::Balanced)]
    pub risk_level: RiskLevel,
    #[arg(long, default_value_t = false)]
    pub execute_orders: bool,
    #[arg(long, default_value_t = false)]
    pub realtime_mode: bool,
    #[arg(long, default_value_t = false)]
    pub simulation_mode: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum RiskLevel {
    Conservative,
    Balanced,
    Aggressive,
}

#[derive(Args)]
pub struct PlanArgs {
    #[arg(long)]
    pub leader_positions_value: Decimal,
    #[arg(long)]
    pub leader_movement_value: Decimal,
}

#[derive(Args)]
pub struct RecordArgs {
    #[arg(long)]
    pub movement_id: String,
    #[arg(long)]
    pub market: String,
    #[arg(long)]
    pub leader_value: Decimal,
    #[arg(long)]
    pub copied_value: Decimal,
    #[arg(long, default_value_t = Decimal::ZERO)]
    pub diff_pct: Decimal,
}

#[derive(Args)]
pub struct SettleArgs {
    #[arg(long)]
    pub movement_id: String,
    #[arg(long)]
    pub pnl: Decimal,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CopyConfig {
    pub leader: String,
    pub allocated_funds: Decimal,
    pub max_trade_pct: Decimal,
    pub max_total_exposure_pct: Decimal,
    pub min_copy_usd: Decimal,
    pub poll_interval_secs: u64,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    pub risk_level: RiskLevel,
    pub execute_orders: bool,
    #[serde(default)]
    pub realtime_mode: bool,
    #[serde(default)]
    pub simulation_mode: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MovementRecord {
    pub movement_id: String,
    pub market: String,
    pub timestamp: String,
    pub leader_value: Decimal,
    pub copied_value: Decimal,
    pub diff_pct: Decimal,
    pub settled: bool,
    pub pnl: Decimal,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct CopyState {
    pub movements: Vec<MovementRecord>,
}

#[derive(Debug, Serialize)]
pub struct PlanResult {
    pub proportional_size: Decimal,
    pub capped_size: Decimal,
    pub available_funds: Decimal,
    pub reason: String,
}

fn default_poll_interval_ms() -> u64 {
    2000
}

fn min_poll_ms(realtime_mode: bool) -> u64 {
    if realtime_mode { 50 } else { 500 }
}

fn normalize_poll_ms(poll_ms: u64, realtime_mode: bool) -> u64 {
    poll_ms.max(min_poll_ms(realtime_mode))
}

pub async fn execute(args: CopyArgs, output: OutputFormat) -> Result<()> {
    match args.command {
        CopyCommand::Configure(cfg) => {
            validate_config(&cfg)?;
            let c = CopyConfig {
                leader: cfg.leader,
                allocated_funds: cfg.allocated_funds,
                max_trade_pct: cfg.max_trade_pct,
                max_total_exposure_pct: cfg.max_total_exposure_pct,
                min_copy_usd: cfg.min_copy_usd,
                poll_interval_secs: cfg.poll_interval_secs,
                poll_interval_ms: normalize_poll_ms(
                    cfg.poll_interval_ms
                        .unwrap_or(cfg.poll_interval_secs.saturating_mul(1000)),
                    cfg.realtime_mode,
                ),
                risk_level: cfg.risk_level,
                execute_orders: cfg.execute_orders,
                realtime_mode: cfg.realtime_mode,
                simulation_mode: cfg.simulation_mode,
            };
            save_config(&c)?;
            init_db(StorageMode::Real)?;
            if matches!(output, OutputFormat::Json) {
                crate::output::print_json(&serde_json::json!({"status": "configured"}))?;
            } else {
                println!("Copy-trader configured successfully.");
            }
            Ok(())
        }
        CopyCommand::Status => {
            let config = load_config()?;
            let state = load_state()?;
            crate::output::copy::print_status(&config, &state, output)
        }
        CopyCommand::Plan(plan_args) => {
            let config = load_config()?;
            let state = load_state()?;
            let result = compute_plan(
                &config,
                &state,
                plan_args.leader_positions_value,
                plan_args.leader_movement_value,
            )?;
            crate::output::copy::print_plan(&result, output)
        }
        CopyCommand::Record(record) => {
            let mut state = load_state()?;
            let entry = MovementRecord {
                movement_id: record.movement_id,
                market: record.market,
                timestamp: Utc::now().to_rfc3339(),
                leader_value: record.leader_value,
                copied_value: record.copied_value,
                diff_pct: record.diff_pct,
                settled: false,
                pnl: Decimal::ZERO,
            };
            state.movements.push(entry.clone());
            save_state(&state)?;
            append_db_movement(current_mode_from_disk(), &entry)?;
            if matches!(output, OutputFormat::Json) {
                crate::output::print_json(&serde_json::json!({"status": "recorded"}))?;
            } else {
                println!("Movement recorded.");
            }
            Ok(())
        }
        CopyCommand::Settle(settle) => {
            let mut state = load_state()?;
            let movement = state
                .movements
                .iter_mut()
                .find(|m| m.movement_id == settle.movement_id)
                .ok_or_else(|| anyhow!("movement not found: {}", settle.movement_id))?;
            movement.settled = true;
            movement.pnl = settle.pnl;
            save_state(&state)?;
            settle_db_movement(current_mode_from_disk(), &settle.movement_id, settle.pnl)?;
            if matches!(output, OutputFormat::Json) {
                crate::output::print_json(&serde_json::json!({"status": "settled"}))?;
            } else {
                println!("Movement settled and funds released.");
            }
            Ok(())
        }
        CopyCommand::Dashboard => {
            let state = load_state()?;
            crate::output::copy::print_dashboard(&state, output)
        }
        CopyCommand::Ui(ui) => run_ui(ui).await,
    }
}

#[derive(Clone)]
struct UiAppState {
    runtime: Arc<Mutex<RuntimeState>>,
}

#[derive(Default)]
struct RuntimeState {
    config: Option<CopyConfig>,
    monitoring: bool,
    current_poll_interval_ms: u64,
    warning: Option<String>,
    last_seen_hashes: HashSet<String>,
    simulation_tick: u64,
}

#[derive(Serialize)]
struct UiStateResponse {
    configured: bool,
    monitoring: bool,
    config: Option<CopyConfig>,
    current_poll_interval_ms: u64,
    warning: Option<String>,
    active_mode: String,
    movement_count: usize,
    initial_allocated_funds: Decimal,
    current_equity: Decimal,
    used_exposure: Decimal,
    available_to_copy: Decimal,
    daily_pnl: Vec<(String, Decimal)>,
    historical_pnl: Vec<(String, Decimal)>,
}

#[derive(Serialize)]
struct UpdatesResponse {
    latest_id: i64,
    movements: Vec<DbMovement>,
}

#[derive(Serialize)]
struct DbMovement {
    id: i64,
    movement_id: String,
    market: String,
    timestamp: String,
    leader_value: String,
    copied_value: String,
    diff_pct: String,
    settled: bool,
    pnl: String,
}

async fn run_ui(ui: UiArgs) -> Result<()> {
    if ui.host != "127.0.0.1" && ui.host != "localhost" {
        bail!("For security, UI host must be 127.0.0.1 or localhost");
    }

    init_db(StorageMode::Real)?;
    let token = generate_api_token()?;
    let addr = format!("{}:{}", ui.host, ui.port);
    println!("Copy UI running at http://{addr}");
    println!("UI API token: {token}");

    let app_state = UiAppState {
        runtime: Arc::new(Mutex::new(RuntimeState {
            config: load_config().ok(),
            monitoring: false,
            current_poll_interval_ms: load_config()
                .ok()
                .map(|c| normalize_poll_ms(c.poll_interval_ms, c.realtime_mode))
                .unwrap_or(default_poll_interval_ms()),
            warning: None,
            last_seen_hashes: HashSet::new(),
            simulation_tick: 0,
        })),
    };

    let listener = TcpListener::bind(&addr)?;
    loop {
        let (stream, _) = listener.accept()?;
        let app = app_state.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let _ = handle_http(stream, app, &token).await;
        });
    }
}

async fn handle_http(mut stream: TcpStream, app: UiAppState, token: &str) -> Result<()> {
    let request = read_http_request(&mut stream)?;
    let (method, path, query) = parse_request_line(&request)?;
    let headers = parse_headers(&request);
    let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
    if path.starts_with("/api/") && !is_authorized(&headers, query, token) {
        write_response(
            &mut stream,
            "401 Unauthorized",
            "application/json",
            "{\"error\":\"unauthorized\"}",
        )?;
        return Ok(());
    }

    match (method, path) {
        ("GET", "/") => write_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            include_str!("../output/copy_ui.html"),
        )?,
        ("GET", "/api/state") => {
            let runtime = app.runtime.lock().await;
            let mode = current_mode_from_runtime(&runtime);
            let db_state = load_state_from_db(mode)?;
            let initial_allocated_funds = runtime
                .config
                .as_ref()
                .map(|c| c.allocated_funds)
                .unwrap_or(Decimal::ZERO);
            let settled_pnl: Decimal = db_state
                .movements
                .iter()
                .filter(|m| m.settled)
                .map(|m| m.pnl)
                .sum();
            let used_exposure: Decimal = db_state
                .movements
                .iter()
                .filter(|m| !m.settled)
                .map(|m| m.copied_value)
                .sum();
            let current_equity = initial_allocated_funds + settled_pnl;
            let available_to_copy = (current_equity - used_exposure).max(Decimal::ZERO);

            let payload = serde_json::to_string(&UiStateResponse {
                configured: runtime.config.is_some(),
                monitoring: runtime.monitoring,
                config: runtime.config.clone(),
                current_poll_interval_ms: runtime.current_poll_interval_ms,
                warning: runtime.warning.clone(),
                active_mode: runtime
                    .config
                    .as_ref()
                    .map(|c| {
                        if c.simulation_mode {
                            "simulacion"
                        } else {
                            "real"
                        }
                    })
                    .unwrap_or("real")
                    .to_string(),
                movement_count: db_state.movements.len(),
                initial_allocated_funds,
                current_equity,
                used_exposure,
                available_to_copy,
                daily_pnl: daily_pnl_series(&db_state.movements),
                historical_pnl: cumulative_pnl_series(&db_state.movements),
            })?;
            write_response(&mut stream, "200 OK", "application/json", &payload)?;
        }
        ("GET", "/api/updates") => {
            let since = parse_since(query);
            let runtime = app.runtime.lock().await;
            let mode = current_mode_from_runtime(&runtime);
            let (latest_id, rows) = db_updates_since(mode, since)?;
            let payload = serde_json::to_string(&UpdatesResponse {
                latest_id,
                movements: rows,
            })?;
            write_response(&mut stream, "200 OK", "application/json", &payload)?;
        }
        ("POST", "/api/configure") => {
            let cfg: ConfigureArgs = serde_json::from_str(body).context("invalid json")?;
            validate_config(&cfg)?;
            let config = CopyConfig {
                leader: cfg.leader,
                allocated_funds: cfg.allocated_funds,
                max_trade_pct: cfg.max_trade_pct,
                max_total_exposure_pct: cfg.max_total_exposure_pct,
                min_copy_usd: cfg.min_copy_usd,
                poll_interval_secs: cfg.poll_interval_secs,
                poll_interval_ms: normalize_poll_ms(
                    cfg.poll_interval_ms
                        .unwrap_or(cfg.poll_interval_secs.saturating_mul(1000)),
                    cfg.realtime_mode,
                ),
                risk_level: cfg.risk_level,
                execute_orders: cfg.execute_orders,
                realtime_mode: cfg.realtime_mode,
                simulation_mode: cfg.simulation_mode,
            };
            save_config(&config)?;
            let mut runtime = app.runtime.lock().await;
            runtime.current_poll_interval_ms = config.poll_interval_ms;
            runtime.config = Some(config);
            write_response(&mut stream, "200 OK", "application/json", "{\"ok\":true}")?;
        }
        ("POST", "/api/start") => {
            {
                let mut runtime = app.runtime.lock().await;
                if runtime.config.is_none() {
                    write_response(
                        &mut stream,
                        "400 Bad Request",
                        "application/json",
                        "{\"error\":\"configure first\"}",
                    )?;
                    return Ok(());
                }
                runtime.monitoring = true;
                let mode = runtime
                    .config
                    .as_ref()
                    .map(|c| if c.simulation_mode { "sim" } else { "real" })
                    .unwrap_or("real");
                log_copy_event(mode, "monitor iniciado");
            }
            let app_clone = app.clone();
            tokio::spawn(async move {
                let _ = monitor_loop(app_clone).await;
            });
            write_response(&mut stream, "200 OK", "application/json", "{\"ok\":true}")?;
        }
        ("POST", "/api/stop") => {
            let mut runtime = app.runtime.lock().await;
            runtime.monitoring = false;
            let mode = runtime
                .config
                .as_ref()
                .map(|c| if c.simulation_mode { "sim" } else { "real" })
                .unwrap_or("real");
            log_copy_event(mode, "monitor detenido");
            write_response(&mut stream, "200 OK", "application/json", "{\"ok\":true}")?;
        }
        _ => write_response(&mut stream, "404 Not Found", "text/plain", "not found")?,
    }

    Ok(())
}

fn log_copy_event(mode: &str, message: impl AsRef<str>) {
    println!("[copy:{mode}] {}", message.as_ref());
}

async fn monitor_loop(app: UiAppState) -> Result<()> {
    let data_client = polymarket_client_sdk::data::Client::default();
    let clob_client = polymarket_client_sdk::clob::Client::default();
    loop {
        let (running, cfg, poll_ms) = {
            let runtime = app.runtime.lock().await;
            (
                runtime.monitoring,
                runtime.config.clone(),
                normalize_poll_ms(
                    runtime.current_poll_interval_ms,
                    runtime
                        .config
                        .as_ref()
                        .map(|c| c.realtime_mode)
                        .unwrap_or(false),
                ),
            )
        };
        if !running {
            break;
        }
        let Some(cfg) = cfg else {
            break;
        };

        if cfg.simulation_mode {
            log_copy_event("sim", format!("tick simulacion (poll={}ms)", poll_ms));
            simulation_step(&app, &cfg, &data_client, &clob_client).await?;
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;
            continue;
        }

        let leader = crate::commands::parse_address(&cfg.leader)?;
        let value_req = ValueRequest::builder().user(leader).build();
        let leader_value = data_client
            .value(&value_req)
            .await
            .ok()
            .and_then(|v| v.first().map(|x| x.value))
            .unwrap_or(Decimal::ONE);

        let settlement_user = if cfg.execute_orders {
            match crate::auth::resolve_signer(None) {
                Ok(signer) => signer.address(),
                Err(e) => {
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning = Some(format!(
                        "execute-orders activo pero no hay wallet configurada: {e}"
                    ));
                    leader
                }
            }
        } else {
            leader
        };

        let closed_req = ClosedPositionsRequest::builder()
            .user(settlement_user)
            .limit(200)?
            .build();
        let closed_positions = match data_client.closed_positions(&closed_req).await {
            Ok(positions) => positions,
            Err(e) => {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(format!("Error consultando posiciones cerradas: {e}"));
                Vec::new()
            }
        };

        if !closed_positions.is_empty() {
            let mut state = load_state()?;
            let settled =
                settle_open_movements_from_closed_positions(&mut state, &closed_positions);
            if !settled.is_empty() {
                save_state(&state)?;
                for movement in settled {
                    log_copy_event(
                        "real",
                        format!(
                            "resuelta {} (mercado={}) pnl={} -> fondos liberados",
                            movement.movement_id, movement.market, movement.pnl
                        ),
                    );
                    settle_db_movement(StorageMode::Real, &movement.movement_id, movement.pnl)?;
                }
            }
        }

        log_copy_event(
            "real",
            "consultando ultimos movimientos del leader (trades recientes)",
        );
        let trades_req = TradesRequest::builder().user(leader).limit(20)?.build();
        let trades = match data_client.trades(&trades_req).await {
            Ok(trades) => {
                log_copy_event(
                    "real",
                    format!("consulta trades completada: {} movimientos", trades.len()),
                );
                let mut runtime = app.runtime.lock().await;
                runtime.warning = None;
                trades
            }
            Err(e) => {
                let mut runtime = app.runtime.lock().await;
                let msg = e.to_string();
                if is_rate_limit_error(&msg) {
                    runtime.current_poll_interval_ms = runtime
                        .current_poll_interval_ms
                        .saturating_add(250)
                        .max(500);
                    runtime.warning = Some(format!(
                        "Rate limit detectado. Aumentando polling a {} ms",
                        runtime.current_poll_interval_ms
                    ));
                } else {
                    runtime.warning = Some(format!("Error consultando trades: {msg}"));
                }
                log_copy_event("real", format!("error consultando trades recientes: {msg}"));
                Vec::new()
            }
        };

        for t in trades {
            let tx_hash = t.transaction_hash.to_string();
            {
                let mut runtime = app.runtime.lock().await;
                if runtime.last_seen_hashes.contains(&tx_hash) {
                    continue;
                }
                runtime.last_seen_hashes.insert(tx_hash.clone());
            }

            let state = load_state()?;
            if state.movements.iter().any(|m| m.movement_id == tx_hash) {
                continue;
            }

            let plan = compute_plan(&cfg, &state, leader_value, t.size * t.price)?;
            if plan.capped_size <= Decimal::ZERO {
                log_copy_event(
                    "real",
                    format!(
                        "trade detectado {} ({}) sin copia (motivo: {})",
                        t.slug, tx_hash, plan.reason
                    ),
                );
                continue;
            }

            log_copy_event(
                "real",
                format!(
                    "nueva apuesta detectada {} ({}) leader_usd={} copia_plan={} motivo={}",
                    t.slug,
                    tx_hash,
                    t.size * t.price,
                    plan.capped_size,
                    plan.reason
                ),
            );

            match has_book_liquidity_for_simulation(&clob_client, &t, plan.capped_size).await {
                Ok(true) => log_copy_event(
                    "real",
                    format!("liquidez disponible para copiar {} ({})", t.slug, tx_hash),
                ),
                Ok(false) => log_copy_event(
                    "real",
                    format!(
                        "sin liquidez suficiente para copiar {} ({})",
                        t.slug, tx_hash
                    ),
                ),
                Err(e) => log_copy_event(
                    "real",
                    format!(
                        "no se pudo validar liquidez para {} ({}): {}",
                        t.slug, tx_hash, e
                    ),
                ),
            }

            if cfg.execute_orders
                && let Err(e) = execute_copy_order_from_trade(&t, plan.capped_size).await
            {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(format!("Error ejecutando orden en wallet: {e}"));
                log_copy_event("real", format!("error copiando orden {}: {e}", tx_hash));
                continue;
            }

            let record = MovementRecord {
                movement_id: tx_hash,
                market: t.slug,
                timestamp: Utc::now().to_rfc3339(),
                leader_value: t.size * t.price,
                copied_value: plan.capped_size,
                diff_pct: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            };
            let mut updated = state;
            updated.movements.push(record.clone());
            save_state(&updated)?;
            append_db_movement(StorageMode::Real, &record)?;
            if cfg.execute_orders {
                log_copy_event(
                    "real",
                    format!("orden copiada {} guardada en historial", record.movement_id),
                );
            } else {
                log_copy_event(
                    "real",
                    format!("orden registrada (dry-run) {}", record.movement_id),
                );
            }
        }

        tokio::time::sleep(Duration::from_millis(poll_ms)).await;
    }
    log_copy_event("core", "monitor loop finalizado");
    Ok(())
}

async fn execute_copy_order_from_trade(
    trade: &polymarket_client_sdk::data::types::response::Trade,
    copied_value_usd: Decimal,
) -> Result<()> {
    let signer = crate::auth::resolve_signer(None)?;
    let client = crate::auth::authenticate_with_signer(&signer, None).await?;

    let side = if trade.side.to_string().eq_ignore_ascii_case("buy") {
        ClobSide::Buy
    } else {
        ClobSide::Sell
    };

    let amount = if matches!(side, ClobSide::Sell) {
        if trade.price <= Decimal::ZERO {
            bail!("invalid leader trade price for sell copy: {}", trade.price);
        }
        let shares = copied_value_usd / trade.price;
        Amount::shares(shares)?
    } else {
        Amount::usdc(copied_value_usd)?
    };

    let order = client
        .market_order()
        .token_id(trade.asset)
        .side(side)
        .amount(amount)
        .order_type(OrderType::FOK)
        .build()
        .await?;
    let signed_order = client.sign(&signer, order).await?;
    let _ = client.post_order(signed_order).await?;
    Ok(())
}

async fn simulation_step(
    app: &UiAppState,
    cfg: &CopyConfig,
    data_client: &polymarket_client_sdk::data::Client,
    clob_client: &polymarket_client_sdk::clob::Client,
) -> Result<()> {
    {
        let mut runtime = app.runtime.lock().await;
        runtime.simulation_tick = runtime.simulation_tick.saturating_add(1);
    }

    let leader = crate::commands::parse_address(&cfg.leader)?;
    let value_req = ValueRequest::builder().user(leader).build();
    let leader_value = data_client
        .value(&value_req)
        .await
        .ok()
        .and_then(|v| v.first().map(|x| x.value))
        .unwrap_or(Decimal::ONE);

    let closed_req = ClosedPositionsRequest::builder()
        .user(leader)
        .limit(200)?
        .build();
    let closed_positions = match data_client.closed_positions(&closed_req).await {
        Ok(positions) => positions,
        Err(e) => {
            let mut runtime = app.runtime.lock().await;
            runtime.warning = Some(format!("Error simulación consultando cerradas: {e}"));
            Vec::new()
        }
    };
    if !closed_positions.is_empty() {
        let mut state = load_state()?;
        let settled = settle_open_movements_from_closed_positions(&mut state, &closed_positions);
        if !settled.is_empty() {
            save_state(&state)?;
            for movement in settled {
                log_copy_event(
                    "sim",
                    format!(
                        "resuelta simulacion {} (mercado={}) pnl={} -> fondos liberados",
                        movement.movement_id, movement.market, movement.pnl
                    ),
                );
                settle_db_movement(StorageMode::Simulation, &movement.movement_id, movement.pnl)?;
            }
        }
    }

    log_copy_event(
        "sim",
        "consultando ultimos movimientos del leader (trades recientes)",
    );
    let trades_req = TradesRequest::builder().user(leader).limit(20)?.build();
    let trades = match data_client.trades(&trades_req).await {
        Ok(trades) => {
            log_copy_event(
                "sim",
                format!("consulta trades completada: {} movimientos", trades.len()),
            );
            trades
        }
        Err(e) => {
            let mut runtime = app.runtime.lock().await;
            runtime.warning = Some(format!("Error simulación consultando trades: {e}"));
            log_copy_event("sim", format!("error consultando trades recientes: {e}"));
            Vec::new()
        }
    };

    for t in trades {
        let tx_hash = t.transaction_hash.to_string();
        {
            let mut runtime = app.runtime.lock().await;
            if runtime.last_seen_hashes.contains(&tx_hash) {
                continue;
            }
            runtime.last_seen_hashes.insert(tx_hash.clone());
        }

        let state = load_state()?;
        let movement_id = format!("sim-{tx_hash}");
        if state.movements.iter().any(|m| m.movement_id == movement_id) {
            continue;
        }

        let plan = compute_plan(cfg, &state, leader_value, t.size * t.price)?;
        if plan.capped_size <= Decimal::ZERO {
            log_copy_event(
                "sim",
                format!(
                    "trade detectado {} ({}) sin simulacion (motivo: {})",
                    t.slug, tx_hash, plan.reason
                ),
            );
            continue;
        }

        log_copy_event(
            "sim",
            format!(
                "nueva apuesta detectada {} ({}) leader_usd={} simulacion_plan={} motivo={}",
                t.slug,
                tx_hash,
                t.size * t.price,
                plan.capped_size,
                plan.reason
            ),
        );

        let has_liquidity =
            has_book_liquidity_for_simulation(clob_client, &t, plan.capped_size).await?;
        log_copy_event(
            "sim",
            format!(
                "chequeo liquidez {} ({}): {}",
                t.slug,
                tx_hash,
                if has_liquidity { "SI" } else { "NO" }
            ),
        );
        if !has_liquidity {
            let mut runtime = app.runtime.lock().await;
            runtime.warning = Some(format!(
                "Simulación: sin liquidez suficiente para {} ({})",
                t.slug, tx_hash
            ));
            log_copy_event(
                "sim",
                format!(
                    "simulacion descartada por liquidez {} ({})",
                    t.slug, tx_hash
                ),
            );
            continue;
        }

        let record = MovementRecord {
            movement_id,
            market: t.slug,
            timestamp: Utc::now().to_rfc3339(),
            leader_value: t.size * t.price,
            copied_value: plan.capped_size,
            diff_pct: Decimal::ZERO,
            settled: false,
            pnl: Decimal::ZERO,
        };
        let mut updated = state;
        updated.movements.push(record.clone());
        save_state(&updated)?;
        append_db_movement(StorageMode::Simulation, &record)?;
        log_copy_event(
            "sim",
            format!("apuesta simulada registrada {}", record.movement_id),
        );
    }

    let mut runtime = app.runtime.lock().await;
    if runtime.warning.is_none() {
        runtime.warning = Some(
            "Modo simulación activo: basado en trades/cierres reales del líder + validación de liquidez"
                .to_string(),
        );
    }
    Ok(())
}

async fn has_book_liquidity_for_simulation(
    clob_client: &polymarket_client_sdk::clob::Client,
    trade: &polymarket_client_sdk::data::types::response::Trade,
    copied_value_usd: Decimal,
) -> Result<bool> {
    let req = OrderBookSummaryRequest::builder()
        .token_id(trade.asset)
        .build();
    let book = clob_client.order_book(&req).await?;

    if trade.side.to_string().eq_ignore_ascii_case("buy") {
        let mut remaining_usdc = copied_value_usd;
        for ask in &book.asks {
            if remaining_usdc <= Decimal::ZERO {
                break;
            }
            let level_notional = ask.size * ask.price;
            if level_notional >= remaining_usdc {
                remaining_usdc = Decimal::ZERO;
                break;
            }
            remaining_usdc -= level_notional;
        }
        Ok(remaining_usdc <= Decimal::ZERO)
    } else {
        if trade.price <= Decimal::ZERO {
            return Ok(false);
        }
        let mut remaining_shares = copied_value_usd / trade.price;
        for bid in &book.bids {
            if remaining_shares <= Decimal::ZERO {
                break;
            }
            if bid.size >= remaining_shares {
                remaining_shares = Decimal::ZERO;
                break;
            }
            remaining_shares -= bid.size;
        }
        Ok(remaining_shares <= Decimal::ZERO)
    }
}

fn is_rate_limit_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("429") || m.contains("too many") || m.contains("rate limit")
}

fn is_authorized(
    headers: &std::collections::HashMap<String, String>,
    query: &str,
    token: &str,
) -> bool {
    let header_ok = headers
        .get("x-api-key")
        .is_some_and(|v| constant_time_eq(v.as_bytes(), token.as_bytes()));
    let query_ok = query
        .split('&')
        .find_map(|kv| kv.split_once('='))
        .is_some_and(|(k, v)| k == "token" && constant_time_eq(v.as_bytes(), token.as_bytes()));

    header_ok || query_ok
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut x = 0u8;
    for (aa, bb) in a.iter().zip(b.iter()) {
        x |= aa ^ bb;
    }
    x == 0
}

fn generate_api_token() -> Result<String> {
    let mut buf = [0u8; 32];

    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        if f.read_exact(&mut buf).is_ok() {
            return Ok(buf.iter().map(|b| format!("{b:02x}")).collect());
        }
    }

    // Cross-platform fallback when /dev/urandom is unavailable (e.g. Windows).
    // Token is only used for local UI auth and remains process-local.
    for i in 0..4u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut hasher);
        std::process::id().hash(&mut hasher);
        i.hash(&mut hasher);
        let block = hasher.finish().to_le_bytes();
        let start = (i as usize) * 8;
        buf[start..start + 8].copy_from_slice(&block);
    }

    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn read_http_request(stream: &mut TcpStream) -> Result<String> {
    let mut buf = vec![0_u8; 1024 * 64];
    let n = stream.read(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf[..n]).to_string())
}

fn parse_request_line(request: &str) -> Result<(&str, &str, &str)> {
    let first = request
        .lines()
        .next()
        .ok_or_else(|| anyhow!("empty request"))?;
    let mut parts = first.split_whitespace();
    let method = parts.next().ok_or_else(|| anyhow!("missing method"))?;
    let target = parts.next().ok_or_else(|| anyhow!("missing path"))?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    Ok((method, path, query))
}

fn parse_headers(request: &str) -> std::collections::HashMap<String, String> {
    let mut headers = std::collections::HashMap::new();
    for line in request.lines().skip(1) {
        if line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    headers
}

fn parse_since(query: &str) -> i64 {
    query
        .split('&')
        .find_map(|kv| kv.split_once('='))
        .and_then(|(k, v)| if k == "since" { v.parse().ok() } else { None })
        .unwrap_or(0)
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes())?;
    Ok(())
}

fn validate_config(cfg: &ConfigureArgs) -> Result<()> {
    if cfg.allocated_funds <= Decimal::ZERO {
        bail!("allocated-funds must be > 0");
    }
    for (name, v) in [
        ("max-trade-pct", cfg.max_trade_pct),
        ("max-total-exposure-pct", cfg.max_total_exposure_pct),
    ] {
        if v <= Decimal::ZERO || v > Decimal::from(100) {
            bail!("{name} must be between 0 and 100");
        }
    }
    if cfg.min_copy_usd < Decimal::ZERO {
        bail!("min-copy-usd cannot be negative");
    }
    if cfg.realtime_mode && cfg.simulation_mode {
        bail!("realtime-mode and simulation-mode are mutually exclusive");
    }
    if let Some(ms) = cfg.poll_interval_ms
        && ms < min_poll_ms(cfg.realtime_mode)
    {
        bail!("poll-interval-ms too low for selected mode");
    }
    Ok(())
}

fn compute_plan(
    cfg: &CopyConfig,
    state: &CopyState,
    leader_positions_value: Decimal,
    leader_movement_value: Decimal,
) -> Result<PlanResult> {
    if leader_positions_value <= Decimal::ZERO {
        bail!("leader-positions-value must be > 0");
    }
    let ratio = cfg.allocated_funds / leader_positions_value;
    let proportional = leader_movement_value * ratio;

    let max_trade = cfg.allocated_funds * (cfg.max_trade_pct / Decimal::from(100));
    let max_total_exposure =
        cfg.allocated_funds * (cfg.max_total_exposure_pct / Decimal::from(100));
    let used_exposure: Decimal = state
        .movements
        .iter()
        .filter(|m| !m.settled)
        .map(|m| m.copied_value)
        .sum();
    let available_exposure = (max_total_exposure - used_exposure).max(Decimal::ZERO);
    let capped = proportional.min(max_trade).min(available_exposure);

    let reason = if capped < cfg.min_copy_usd {
        "below minimum copy threshold".to_string()
    } else if available_exposure <= Decimal::ZERO {
        "no exposure available".to_string()
    } else if proportional > max_trade {
        "capped by max_trade_pct".to_string()
    } else if proportional > available_exposure {
        "capped by max_total_exposure_pct".to_string()
    } else {
        "ok".to_string()
    };

    Ok(PlanResult {
        proportional_size: proportional,
        capped_size: if reason == "below minimum copy threshold" {
            Decimal::ZERO
        } else {
            capped
        },
        available_funds: available_exposure,
        reason,
    })
}

fn roi_by_slug_from_totals<'a>(
    totals: HashMap<&'a str, (Decimal, Decimal)>,
) -> HashMap<&'a str, Decimal> {
    totals
        .into_iter()
        .filter_map(|(slug, (realized, bought))| {
            if bought <= Decimal::ZERO {
                None
            } else {
                Some((slug, realized / bought))
            }
        })
        .collect()
}

fn settle_open_movements_from_closed_positions(
    state: &mut CopyState,
    closed_positions: &[polymarket_client_sdk::data::types::response::ClosedPosition],
) -> Vec<MovementRecord> {
    let mut realized_and_bought_by_slug: HashMap<&str, (Decimal, Decimal)> = HashMap::new();
    for closed in closed_positions {
        let entry = realized_and_bought_by_slug
            .entry(closed.slug.as_str())
            .or_insert((Decimal::ZERO, Decimal::ZERO));
        entry.0 += closed.realized_pnl;
        entry.1 += closed.total_bought;
    }

    let roi_by_slug = roi_by_slug_from_totals(realized_and_bought_by_slug);

    let mut settled = Vec::new();
    for movement in state.movements.iter_mut().filter(|m| !m.settled) {
        let Some(roi) = roi_by_slug.get(movement.market.as_str()) else {
            continue;
        };
        movement.pnl = movement.copied_value * *roi;
        movement.settled = true;
        settled.push(movement.clone());
    }

    settled
}

fn base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not determine home directory")?;
    Ok(home.join(".config").join("polymarket"))
}

fn config_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("copy_trader.json"))
}

fn state_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("copy_trader_state.json"))
}

fn db_path(mode: StorageMode) -> Result<PathBuf> {
    let filename = match mode {
        StorageMode::Real => "copy_trader_real_db.jsonl",
        StorageMode::Simulation => "copy_trader_sim_db.jsonl",
    };
    Ok(base_dir()?.join(filename))
}

fn init_db(mode: StorageMode) -> Result<()> {
    let path = db_path(mode)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(path, "")?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StorageMode {
    Real,
    Simulation,
}

fn mode_from_simulation(simulation_mode: bool) -> StorageMode {
    if simulation_mode {
        StorageMode::Simulation
    } else {
        StorageMode::Real
    }
}

fn mode_from_config(cfg: &CopyConfig) -> StorageMode {
    mode_from_simulation(cfg.simulation_mode)
}

fn current_mode_from_runtime(runtime: &RuntimeState) -> StorageMode {
    runtime
        .config
        .as_ref()
        .map(mode_from_config)
        .unwrap_or(StorageMode::Real)
}

fn current_mode_from_disk() -> StorageMode {
    load_config()
        .map(|c| mode_from_config(&c))
        .unwrap_or(StorageMode::Real)
}

#[derive(Serialize, Deserialize)]
struct DbRow {
    id: i64,
    movement_id: String,
    market: String,
    timestamp: String,
    leader_value: String,
    copied_value: String,
    diff_pct: String,
    settled: bool,
    pnl: String,
}

fn next_db_id(rows: &[DbRow]) -> i64 {
    rows.last().map_or(1, |r| r.id + 1)
}

fn read_db_rows(mode: StorageMode) -> Result<Vec<DbRow>> {
    init_db(mode)?;
    let raw = fs::read_to_string(db_path(mode)?)?;
    let mut out = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(v) = serde_json::from_str::<DbRow>(line) {
            out.push(v);
        }
    }
    out.sort_by_key(|x| x.id);
    Ok(out)
}

fn write_db_rows(mode: StorageMode, rows: &[DbRow]) -> Result<()> {
    let mut body = String::new();
    for r in rows {
        body.push_str(&serde_json::to_string(r)?);
        body.push('\n');
    }
    fs::write(db_path(mode)?, body)?;
    Ok(())
}

fn append_db_movement(mode: StorageMode, m: &MovementRecord) -> Result<()> {
    let mut rows = read_db_rows(mode)?;
    if rows.iter().any(|r| r.movement_id == m.movement_id) {
        return Ok(());
    }
    rows.push(DbRow {
        id: next_db_id(&rows),
        movement_id: m.movement_id.clone(),
        market: m.market.clone(),
        timestamp: m.timestamp.clone(),
        leader_value: m.leader_value.to_string(),
        copied_value: m.copied_value.to_string(),
        diff_pct: m.diff_pct.to_string(),
        settled: m.settled,
        pnl: m.pnl.to_string(),
    });
    write_db_rows(mode, &rows)
}

fn settle_db_movement(mode: StorageMode, movement_id: &str, pnl: Decimal) -> Result<()> {
    let mut rows = read_db_rows(mode)?;
    for r in &mut rows {
        if r.movement_id == movement_id {
            r.settled = true;
            r.pnl = pnl.to_string();
        }
    }
    write_db_rows(mode, &rows)
}

fn load_state_from_db(mode: StorageMode) -> Result<CopyState> {
    let rows = read_db_rows(mode)?;
    let movements = rows
        .into_iter()
        .map(|r| MovementRecord {
            movement_id: r.movement_id,
            market: r.market,
            timestamp: r.timestamp,
            leader_value: Decimal::from_str_exact(&r.leader_value).unwrap_or(Decimal::ZERO),
            copied_value: Decimal::from_str_exact(&r.copied_value).unwrap_or(Decimal::ZERO),
            diff_pct: Decimal::from_str_exact(&r.diff_pct).unwrap_or(Decimal::ZERO),
            settled: r.settled,
            pnl: Decimal::from_str_exact(&r.pnl).unwrap_or(Decimal::ZERO),
        })
        .collect();
    Ok(CopyState { movements })
}

fn db_updates_since(mode: StorageMode, since: i64) -> Result<(i64, Vec<DbMovement>)> {
    let rows = read_db_rows(mode)?;
    let latest_id = rows.last().map_or(0, |r| r.id);
    let updates = rows
        .into_iter()
        .filter(|r| r.id > since)
        .take(200)
        .map(|r| DbMovement {
            id: r.id,
            movement_id: r.movement_id,
            market: r.market,
            timestamp: r.timestamp,
            leader_value: r.leader_value,
            copied_value: r.copied_value,
            diff_pct: r.diff_pct,
            settled: r.settled,
            pnl: r.pnl,
        })
        .collect();
    Ok((latest_id, updates))
}

fn save_config(cfg: &CopyConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(cfg)?)?;
    Ok(())
}

fn load_config() -> Result<CopyConfig> {
    let data = fs::read_to_string(config_path()?)
        .context("Copy-trader is not configured. Run `polymarket copy configure ...`")?;
    serde_json::from_str(&data).context("Invalid copy-trader config")
}

fn save_state(state: &CopyState) -> Result<()> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(state)?)?;
    Ok(())
}

fn load_state() -> Result<CopyState> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(CopyState::default());
    }
    let data = fs::read_to_string(path)?;
    serde_json::from_str(&data).context("Invalid copy-trader state")
}

pub fn daily_pnl_series(movements: &[MovementRecord]) -> Vec<(String, Decimal)> {
    let mut by_day: BTreeMap<String, Decimal> = BTreeMap::new();
    for m in movements.iter().filter(|m| m.settled) {
        let day = m.timestamp.get(0..10).unwrap_or("unknown").to_string();
        by_day
            .entry(day)
            .and_modify(|x| *x += m.pnl)
            .or_insert(m.pnl);
    }
    by_day.into_iter().collect()
}

pub fn cumulative_pnl_series(movements: &[MovementRecord]) -> Vec<(String, Decimal)> {
    let mut cumulative = Decimal::ZERO;
    daily_pnl_series(movements)
        .into_iter()
        .map(|(day, pnl)| {
            cumulative += pnl;
            (day, cumulative)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(v: &str) -> Decimal {
        Decimal::from_str(v).unwrap()
    }

    #[test]
    fn plan_is_capped_by_max_trade() {
        let cfg = CopyConfig {
            leader: "0x1".into(),
            allocated_funds: d("1000"),
            max_trade_pct: d("5"),
            max_total_exposure_pct: d("100"),
            min_copy_usd: d("1"),
            poll_interval_secs: 2,
            poll_interval_ms: 2000,
            risk_level: RiskLevel::Balanced,
            execute_orders: false,
            realtime_mode: false,
            simulation_mode: false,
        };
        let state = CopyState::default();
        let p = compute_plan(&cfg, &state, d("1000"), d("200")).unwrap();
        assert_eq!(p.capped_size, d("50"));
        assert_eq!(p.reason, "capped by max_trade_pct");
    }

    #[test]
    fn plan_respects_total_exposure_limit() {
        let cfg = CopyConfig {
            leader: "0x1".into(),
            allocated_funds: d("1000"),
            max_trade_pct: d("50"),
            max_total_exposure_pct: d("60"),
            min_copy_usd: d("1"),
            poll_interval_secs: 2,
            poll_interval_ms: 2000,
            risk_level: RiskLevel::Balanced,
            execute_orders: false,
            realtime_mode: false,
            simulation_mode: false,
        };
        let state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "a".into(),
                market: "m".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
                leader_value: d("100"),
                copied_value: d("550"),
                diff_pct: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };
        let p = compute_plan(&cfg, &state, d("1000"), d("100")).unwrap();
        assert_eq!(p.capped_size, d("50"));
        assert_eq!(p.available_funds, d("50"));
    }

    #[test]
    fn roi_by_slug_from_totals_computes_weighted_roi() {
        let mut totals = HashMap::new();
        totals.insert("btc-up", (d("25"), d("100")));
        totals.insert("eth-down", (d("-10"), d("40")));
        totals.insert("skip", (d("5"), d("0")));

        let roi = roi_by_slug_from_totals(totals);
        assert_eq!(roi.get("btc-up"), Some(&d("0.25")));
        assert_eq!(roi.get("eth-down"), Some(&d("-0.25")));
        assert!(!roi.contains_key("skip"));
    }
}
