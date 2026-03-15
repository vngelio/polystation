use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
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
use polymarket_client_sdk::data::types::ActivityType;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::output::OutputFormat;
use polymarket_client_sdk::auth::Signer as _;
use polymarket_client_sdk::clob::types::request::OrderBookSummaryRequest;
use polymarket_client_sdk::clob::types::{Amount, OrderType, Side as ClobSide};
use polymarket_client_sdk::data::types::request::{
    ActivityRequest, ClosedPositionsRequest, TradesRequest, ValueRequest,
};
use polymarket_client_sdk::gamma::types::request::MarketsRequest;

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
    #[serde(default)]
    pub leader_price: Decimal,
    pub copied_value: Decimal,
    #[serde(default)]
    pub simulated_copy_price: Decimal,
    #[serde(default)]
    pub quantity: Decimal,
    #[serde(default)]
    pub copy_side: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub resolved_outcome: String,
    pub diff_pct: Decimal,
    #[serde(default)]
    pub estimated_total_fee_usd: Decimal,
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

fn min_poll_ms(realtime_mode: bool, simulation_mode: bool) -> u64 {
    if realtime_mode || simulation_mode {
        50
    } else {
        500
    }
}

fn normalize_poll_ms(poll_ms: u64, realtime_mode: bool, simulation_mode: bool) -> u64 {
    poll_ms.max(min_poll_ms(realtime_mode, simulation_mode))
}

const FAST_MARKET_FEE_BPS: u32 = 70;
const BPS_DENOMINATOR: u32 = 10_000;

fn is_fast_market_with_fee(slug: &str) -> bool {
    let normalized = normalize_market_slug(slug);
    normalized.contains("-updown-5m") || normalized.contains("-updown-15m")
}

fn trading_fee_impact_for_movement(
    market: &str,
    copied_value: Decimal,
) -> Option<TradingFeeImpact> {
    if !is_fast_market_with_fee(market) || copied_value <= Decimal::ZERO {
        return None;
    }

    let fee_rate = Decimal::from(FAST_MARKET_FEE_BPS) / Decimal::from(BPS_DENOMINATOR);
    let entry_fee_usd = copied_value * fee_rate;
    let round_trip_fee_usd = entry_fee_usd * Decimal::from(2);
    let max_gross_profit_usd =
        copied_value * (Decimal::ONE - Decimal::from_i128_with_scale(100, 3));
    let max_net_profit_usd = max_gross_profit_usd - round_trip_fee_usd;

    Some(TradingFeeImpact {
        fee_bps: FAST_MARKET_FEE_BPS,
        entry_fee_usd,
        round_trip_fee_usd,
        max_gross_profit_usd,
        max_net_profit_usd,
    })
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
                    cfg.simulation_mode,
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
                leader_price: Decimal::ZERO,
                copied_value: record.copied_value,
                simulated_copy_price: Decimal::ZERO,
                quantity: Decimal::ZERO,
                copy_side: "unknown".to_string(),
                outcome: String::new(),
                resolved_outcome: String::new(),
                diff_pct: record.diff_pct,
                estimated_total_fee_usd: Decimal::ZERO,
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
            let movement_for_log = movement.clone();
            save_state(&state)?;
            let mode = current_mode_from_disk();
            settle_db_movement(mode, &settle.movement_id, settle.pnl)?;
            if let Err(e) = append_settlement_log(mode, &movement_for_log) {
                eprintln!("warning: could not append settlement log: {e}");
            }
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
    last_seen_trade_keys_real: HashSet<String>,
    last_seen_trade_keys_sim: HashSet<String>,
    simulation_tick: u64,
    next_closed_sync_real_at_ms: i64,
    next_closed_sync_sim_at_ms: i64,
    closed_sync_backoff_real_ms: u64,
    closed_sync_backoff_sim_ms: u64,
    closed_sync_real_in_flight: bool,
    closed_sync_sim_in_flight: bool,
    next_market_sync_real_at_ms: i64,
    next_market_sync_sim_at_ms: i64,
    market_sync_backoff_real_ms: u64,
    market_sync_backoff_sim_ms: u64,
    market_sync_real_in_flight: bool,
    market_sync_sim_in_flight: bool,
    simulation_bootstrap_done: bool,
    simulation_bootstrap_next_retry_at_ms: i64,
}

const CLOSED_SYNC_BASE_MS: u64 = 30_000;
const CLOSED_SYNC_MAX_BACKOFF_MS: u64 = 30_000;
const MARKET_SYNC_BASE_MS: u64 = 30_000;
const MARKET_SYNC_MAX_BACKOFF_MS: u64 = 120_000;
const SIM_BOOTSTRAP_RETRY_MS: u64 = 300_000;

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
    recent_movements: Vec<DbMovement>,
}

#[derive(Serialize)]
struct UpdatesResponse {
    latest_id: i64,
    movements: Vec<DbMovement>,
}

#[derive(Serialize, Clone)]
struct DbMovement {
    id: i64,
    movement_id: String,
    market: String,
    timestamp: String,
    leader_value: String,
    #[serde(default)]
    leader_price: String,
    copied_value: String,
    #[serde(default)]
    simulated_copy_price: String,
    #[serde(default)]
    quantity: String,
    #[serde(default)]
    copy_side: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    resolved_outcome: String,
    diff_pct: String,
    #[serde(default)]
    estimated_total_fee_usd: String,
    settled: bool,
    pnl: String,
}

#[derive(Debug, Clone, Copy)]
struct TradingFeeImpact {
    fee_bps: u32,
    entry_fee_usd: Decimal,
    round_trip_fee_usd: Decimal,
    max_gross_profit_usd: Decimal,
    max_net_profit_usd: Decimal,
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
                .map(|c| normalize_poll_ms(c.poll_interval_ms, c.realtime_mode, c.simulation_mode))
                .unwrap_or(default_poll_interval_ms()),
            warning: None,
            last_seen_trade_keys_real: HashSet::new(),
            last_seen_trade_keys_sim: HashSet::new(),
            simulation_tick: 0,
            next_closed_sync_real_at_ms: 0,
            next_closed_sync_sim_at_ms: 0,
            closed_sync_backoff_real_ms: CLOSED_SYNC_BASE_MS,
            closed_sync_backoff_sim_ms: CLOSED_SYNC_BASE_MS,
            closed_sync_real_in_flight: false,
            closed_sync_sim_in_flight: false,
            next_market_sync_real_at_ms: 0,
            next_market_sync_sim_at_ms: 0,
            market_sync_backoff_real_ms: MARKET_SYNC_BASE_MS,
            market_sync_backoff_sim_ms: MARKET_SYNC_BASE_MS,
            market_sync_real_in_flight: false,
            market_sync_sim_in_flight: false,
            simulation_bootstrap_done: false,
            simulation_bootstrap_next_retry_at_ms: 0,
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
            let settled_pnl_after_fees: Decimal = db_state
                .movements
                .iter()
                .filter(|m| m.settled)
                .map(|m| m.pnl - m.estimated_total_fee_usd)
                .sum();
            let used_exposure: Decimal = db_state
                .movements
                .iter()
                .filter(|m| !m.settled)
                .map(|m| m.copied_value)
                .sum();
            let current_equity = initial_allocated_funds + settled_pnl_after_fees;
            let available_to_copy = (current_equity - used_exposure).max(Decimal::ZERO);

            let (_, mut recent_rows) = db_updates_since(mode, 0)?;
            if recent_rows.len() > 300 {
                recent_rows = recent_rows[recent_rows.len().saturating_sub(300)..].to_vec();
            }

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
                recent_movements: recent_rows,
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
                    cfg.simulation_mode,
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
                runtime.simulation_bootstrap_done = false;
                runtime.simulation_bootstrap_next_retry_at_ms = 0;
                runtime.last_seen_trade_keys_real.clear();
                runtime.last_seen_trade_keys_sim.clear();
                let mode = runtime
                    .config
                    .as_ref()
                    .map(|c| if c.simulation_mode { "sim" } else { "real" })
                    .unwrap_or("real");
                log_copy_event(mode, "monitor iniciado");
            }
            let app_clone = app.clone();
            tokio::spawn(async move {
                if let Err(e) = monitor_loop(app_clone).await {
                    log_copy_event("core", format!("monitor loop finalizado con error: {e}"));
                }
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
    let msg = message.as_ref();
    println!("[copy:{mode}] {msg}");

    if !should_persist_copy_log_message(msg) {
        return;
    }

    let ts = Utc::now().to_rfc3339();
    let line = format!(
        "{ts}	mode={mode}	{msg}
"
    );

    let mut paths = vec![PathBuf::from("copy_trader.log")];
    if let Ok(path) = base_dir().map(|d| d.join("copy_trader.log")) {
        paths.push(path);
    }

    for path in paths {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && fs::create_dir_all(parent).is_err()
        {
            continue;
        }
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn should_persist_copy_log_message(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();

    // Avoid high-frequency noise in file logs (polling/query heartbeat).
    if m.contains("consultando")
        || m.contains("consulta trades completada")
        || m.contains("consulta de cierres completada")
        || m.contains("timeout consultando")
        || m.contains("tick simulacion")
        || m.contains("ciclo monitor")
    {
        return false;
    }

    true
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn closed_sync_due(next_at_ms: i64) -> bool {
    now_ms() >= next_at_ms
}

fn schedule_closed_sync_success(runtime: &mut RuntimeState, mode: StorageMode) {
    match mode {
        StorageMode::Real => {
            runtime.closed_sync_backoff_real_ms = CLOSED_SYNC_BASE_MS;
            runtime.next_closed_sync_real_at_ms =
                now_ms() + i64::try_from(CLOSED_SYNC_BASE_MS).unwrap_or(5_000);
        }
        StorageMode::Simulation => {
            runtime.closed_sync_backoff_sim_ms = CLOSED_SYNC_BASE_MS;
            runtime.next_closed_sync_sim_at_ms =
                now_ms() + i64::try_from(CLOSED_SYNC_BASE_MS).unwrap_or(5_000);
        }
    }
}

fn schedule_closed_sync_backoff(runtime: &mut RuntimeState, mode: StorageMode) {
    match mode {
        StorageMode::Real => {
            let current = runtime.closed_sync_backoff_real_ms.max(CLOSED_SYNC_BASE_MS);
            let next = (current.saturating_mul(2)).min(CLOSED_SYNC_MAX_BACKOFF_MS);
            runtime.closed_sync_backoff_real_ms = next;
            runtime.next_closed_sync_real_at_ms = now_ms() + i64::try_from(next).unwrap_or(30_000);
        }
        StorageMode::Simulation => {
            let current = runtime.closed_sync_backoff_sim_ms.max(CLOSED_SYNC_BASE_MS);
            let next = (current.saturating_mul(2)).min(CLOSED_SYNC_MAX_BACKOFF_MS);
            runtime.closed_sync_backoff_sim_ms = next;
            runtime.next_closed_sync_sim_at_ms = now_ms() + i64::try_from(next).unwrap_or(30_000);
        }
    }
}

fn schedule_market_sync_success(runtime: &mut RuntimeState, mode: StorageMode) {
    match mode {
        StorageMode::Real => {
            runtime.market_sync_backoff_real_ms = MARKET_SYNC_BASE_MS;
            runtime.next_market_sync_real_at_ms =
                now_ms() + i64::try_from(MARKET_SYNC_BASE_MS).unwrap_or(30_000);
        }
        StorageMode::Simulation => {
            runtime.market_sync_backoff_sim_ms = MARKET_SYNC_BASE_MS;
            runtime.next_market_sync_sim_at_ms =
                now_ms() + i64::try_from(MARKET_SYNC_BASE_MS).unwrap_or(30_000);
        }
    }
}

fn schedule_market_sync_backoff(runtime: &mut RuntimeState, mode: StorageMode) {
    match mode {
        StorageMode::Real => {
            let current = runtime.market_sync_backoff_real_ms.max(MARKET_SYNC_BASE_MS);
            let next = (current.saturating_mul(2)).min(MARKET_SYNC_MAX_BACKOFF_MS);
            runtime.market_sync_backoff_real_ms = next;
            runtime.next_market_sync_real_at_ms = now_ms() + i64::try_from(next).unwrap_or(120_000);
        }
        StorageMode::Simulation => {
            let current = runtime.market_sync_backoff_sim_ms.max(MARKET_SYNC_BASE_MS);
            let next = (current.saturating_mul(2)).min(MARKET_SYNC_MAX_BACKOFF_MS);
            runtime.market_sync_backoff_sim_ms = next;
            runtime.next_market_sync_sim_at_ms = now_ms() + i64::try_from(next).unwrap_or(120_000);
        }
    }
}

async fn monitor_loop(app: UiAppState) -> Result<()> {
    let data_client = polymarket_client_sdk::data::Client::default();
    let clob_client = polymarket_client_sdk::clob::Client::default();
    let mut loop_tick: u64 = 0;
    loop {
        loop_tick = loop_tick.saturating_add(1);
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
                    runtime
                        .config
                        .as_ref()
                        .map(|c| c.simulation_mode)
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

        log_copy_event(
            "core",
            format!(
                "ciclo monitor #{loop_tick} iniciado (mode={}, poll={}ms)",
                if cfg.simulation_mode { "sim" } else { "real" },
                poll_ms
            ),
        );

        if cfg.simulation_mode {
            log_copy_event("sim", format!("tick simulacion (poll={}ms)", poll_ms));
            if let Err(e) = simulation_step(&app, &cfg, &data_client, &clob_client).await {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(format!("Error en tick simulación: {e}"));
                log_copy_event("sim", format!("tick simulación con error: {e}"));
            }
            log_copy_event(
                "core",
                format!("ciclo monitor #{loop_tick} finalizado; esperando {poll_ms}ms"),
            );
            tokio::time::sleep(Duration::from_millis(poll_ms)).await;
            continue;
        }

        let leader = match crate::commands::parse_address(&cfg.leader) {
            Ok(addr) => addr,
            Err(e) => {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(format!("Leader inválido: {e}"));
                log_copy_event("real", format!("error parseando leader: {e}"));
                tokio::time::sleep(Duration::from_millis(poll_ms)).await;
                continue;
            }
        };
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

        let mut remaining_wallet_value_usd = if cfg.execute_orders {
            let wallet_value_req = ValueRequest::builder().user(settlement_user).build();
            match tokio::time::timeout(
                Duration::from_secs(15),
                data_client.value(&wallet_value_req),
            )
            .await
            {
                Ok(Ok(v)) => {
                    let total = v.first().map(|x| x.value).unwrap_or(Decimal::ZERO);
                    log_copy_event(
                        "real",
                        format!(
                            "valor actual wallet ejecutora {}: {} USD",
                            settlement_user, total
                        ),
                    );
                    Some(total)
                }
                Ok(Err(e)) => {
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning = Some(format!(
                        "No se pudo validar fondos de wallet ejecutora: {e}"
                    ));
                    log_copy_event(
                        "real",
                        format!(
                            "error consultando valor wallet ejecutora {}: {}",
                            settlement_user, e
                        ),
                    );
                    None
                }
                Err(_) => {
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning =
                        Some("Timeout validando fondos de wallet ejecutora".to_string());
                    log_copy_event(
                        "real",
                        format!(
                            "timeout consultando valor wallet ejecutora {} (15s)",
                            settlement_user
                        ),
                    );
                    None
                }
            }
        } else {
            None
        };

        let should_sync_closed = {
            let runtime = app.runtime.lock().await;
            closed_sync_due(runtime.next_closed_sync_real_at_ms)
                && !runtime.closed_sync_real_in_flight
        };

        if should_sync_closed {
            {
                let mut runtime = app.runtime.lock().await;
                runtime.closed_sync_real_in_flight = true;
            }
            let app_bg = app.clone();
            tokio::spawn(async move {
                run_closed_sync_task(app_bg, settlement_user, StorageMode::Real, "real").await;
            });
        }

        let should_sync_market = {
            let runtime = app.runtime.lock().await;
            closed_sync_due(runtime.next_market_sync_real_at_ms)
                && !runtime.market_sync_real_in_flight
        };

        if should_sync_market {
            {
                let mut runtime = app.runtime.lock().await;
                runtime.market_sync_real_in_flight = true;
            }
            let app_bg = app.clone();
            tokio::spawn(async move {
                run_market_closed_sync_task(app_bg, settlement_user, StorageMode::Real, "real")
                    .await;
            });
        }

        log_copy_event(
            "real",
            format!("consultando ultimos movimientos de la cuenta a copiar ({leader})"),
        );
        let trades_req = TradesRequest::builder().user(leader).limit(20)?.build();
        let trades =
            match tokio::time::timeout(Duration::from_secs(15), data_client.trades(&trades_req))
                .await
            {
                Ok(Ok(trades)) => {
                    log_copy_event(
                        "real",
                        format!("consulta trades completada: {} movimientos", trades.len()),
                    );
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning = None;
                    trades
                }
                Ok(Err(e)) => {
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
                Err(_) => {
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning = Some("Timeout consultando trades recientes".to_string());
                    log_copy_event("real", "timeout consultando ultimos movimientos (15s)");
                    Vec::new()
                }
            };

        let prime_only = {
            let mut runtime = app.runtime.lock().await;
            if runtime.last_seen_trade_keys_real.is_empty() {
                for t in &trades {
                    runtime.last_seen_trade_keys_real.insert(trade_event_key(t));
                }
                true
            } else {
                false
            }
        };

        if prime_only {
            log_copy_event(
                "real",
                format!(
                    "primer barrido: {} trades marcados como vistos (sin copiar histórico)",
                    trades.len()
                ),
            );
            return Ok(());
        }

        for t in trades {
            let tx_hash = t.transaction_hash.to_string();
            let trade_key = trade_event_key(&t);
            let movement_id = format!("real-{trade_key}");
            let is_sell = t.side.to_string().eq_ignore_ascii_case("sell");
            {
                let mut runtime = app.runtime.lock().await;
                if runtime.last_seen_trade_keys_real.contains(&trade_key) {
                    continue;
                }
                if !is_sell {
                    runtime.last_seen_trade_keys_real.insert(trade_key.clone());
                }
            }

            let mut state = load_state()?;
            if state.movements.iter().any(|m| m.movement_id == movement_id) {
                continue;
            }

            if is_sell {
                let settled_from_sell =
                    settle_open_buys_from_sell_trade(&mut state, &t.slug, &t.outcome, t.price);
                if !settled_from_sell.is_empty() {
                    save_state(&state)?;
                    for movement in settled_from_sell {
                        settle_db_movement(StorageMode::Real, &movement.movement_id, movement.pnl)?;
                        if let Err(e) = append_settlement_log(StorageMode::Real, &movement) {
                            log_copy_event(
                                "real",
                                format!("error escribiendo log de settlement: {e}"),
                            );
                        }
                        log_copy_event(
                            "real",
                            format!(
                                "sell líder detectado: cerrada {} (mercado={}, outcome={}) pnl={} por precio de salida {}",
                                movement.movement_id,
                                movement.market,
                                movement.outcome,
                                movement.pnl,
                                t.price
                            ),
                        );
                    }
                    let mut runtime = app.runtime.lock().await;
                    runtime.last_seen_trade_keys_real.insert(trade_key.clone());
                    continue;
                }
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

            if t.side.to_string().eq_ignore_ascii_case("sell") {
                let required_sell_shares = copied_shares_from_notional(plan.capped_size, t.price);
                if !has_enough_inventory_for_sell(&state, &t.slug, &t.outcome, required_sell_shares)
                {
                    log_copy_event(
                        "real",
                        format!(
                            "sell {} ({}) descartado: no hay buy abierto conciliable (outcome={}, required_shares={})",
                            t.slug, tx_hash, t.outcome, required_sell_shares
                        ),
                    );
                    continue;
                }

                // If this path is reached, sell did not close previous buys via immediate settlement.
                // Avoid creating open SELL rows; SELL must always close an existing BUY.
                log_copy_event(
                    "real",
                    format!(
                        "sell {} ({}) descartado: no se pudo conciliar cierre inmediato; evitando SELL abierto",
                        t.slug, tx_hash
                    ),
                );
                continue;
            }

            let fee_impact = trading_fee_impact_for_movement(&t.slug, plan.capped_size);
            if let Some(impact) = fee_impact
                && impact.max_net_profit_usd <= Decimal::ZERO
            {
                log_copy_event(
                    "real",
                    format!(
                        "trade {} ({}) descartado por fees ({} bps): profit_max_neto={} (gross_max={} fee_entry={} fees_rt={})",
                        t.slug,
                        tx_hash,
                        impact.fee_bps,
                        impact.max_net_profit_usd,
                        impact.max_gross_profit_usd,
                        impact.entry_fee_usd,
                        impact.round_trip_fee_usd,
                    ),
                );
                continue;
            }

            log_copy_event(
                "real",
                format!(
                    "nueva apuesta detectada {} ({}) side={} outcome={} leader_usd={} leader_price={} cantidad={} copia_plan={} sim_price={} motivo={}",
                    t.slug,
                    tx_hash,
                    t.side,
                    t.outcome,
                    t.size * t.price,
                    t.price,
                    t.size,
                    plan.capped_size,
                    t.price,
                    plan.reason
                ),
            );

            let (estimated_sim_price, has_full_liquidity) =
                match estimate_simulated_copy_price_from_book(&clob_client, &t, plan.capped_size)
                    .await
                {
                    Ok((Some(px), full_fill)) => {
                        if full_fill {
                            log_copy_event(
                                "real",
                                format!(
                                    "liquidez disponible para copiar {} ({}) px_sim={}",
                                    t.slug, tx_hash, px
                                ),
                            );
                        } else {
                            log_copy_event(
                                "real",
                                format!(
                                    "liquidez parcial para copiar {} ({}) px_sim={} (estimación con fill parcial)",
                                    t.slug, tx_hash, px
                                ),
                            );
                        }
                        (Some(px), full_fill)
                    }
                    Ok((None, _)) => {
                        log_copy_event(
                            "real",
                            format!(
                                "sin liquidez suficiente para copiar {} ({})",
                                t.slug, tx_hash
                            ),
                        );
                        (None, false)
                    }
                    Err(e) => {
                        log_copy_event(
                            "real",
                            format!(
                                "no se pudo validar liquidez para {} ({}): {}",
                                t.slug, tx_hash, e
                            ),
                        );
                        (None, false)
                    }
                };

            if cfg.execute_orders {
                let Some(wallet_available) = remaining_wallet_value_usd else {
                    log_copy_event(
                        "real",
                        format!(
                            "orden {} omitida: no se pudo validar balance real de wallet",
                            tx_hash
                        ),
                    );
                    continue;
                };

                if wallet_available < plan.capped_size {
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning = Some(format!(
                        "Fondos insuficientes en wallet ejecutora: disponible={} requerido={}",
                        wallet_available, plan.capped_size
                    ));
                    log_copy_event(
                        "real",
                        format!(
                            "orden {} omitida por fondos insuficientes (disponible={} requerido={})",
                            tx_hash, wallet_available, plan.capped_size
                        ),
                    );
                    continue;
                }

                if let Err(e) = execute_copy_order_from_trade(&t, plan.capped_size).await {
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning = Some(format!("Error ejecutando orden en wallet: {e}"));
                    log_copy_event("real", format!("error copiando orden {}: {e}", tx_hash));
                    continue;
                }

                remaining_wallet_value_usd =
                    Some((wallet_available - plan.capped_size).max(Decimal::ZERO));
            }

            if !has_full_liquidity {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(format!(
                    "Liquidez parcial en {} ({}), estimación de precio con fill parcial",
                    t.slug, tx_hash
                ));
            }

            let record = MovementRecord {
                movement_id: movement_id.clone(),
                market: t.slug,
                timestamp: Utc::now().to_rfc3339(),
                leader_value: t.size * t.price,
                leader_price: t.price,
                copied_value: plan.capped_size,
                simulated_copy_price: estimated_sim_price.unwrap_or(Decimal::ZERO),
                quantity: t.size,
                copy_side: t.side.to_string(),
                outcome: t.outcome.clone(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: fee_impact
                    .map(|x| x.round_trip_fee_usd)
                    .unwrap_or(Decimal::ZERO),
                settled: false,
                pnl: Decimal::ZERO,
            };
            let mut updated = state;
            updated.movements.push(record.clone());
            save_state(&updated)?;
            append_db_movement(StorageMode::Real, &record)?;
            if is_sell {
                let mut runtime = app.runtime.lock().await;
                runtime.last_seen_trade_keys_real.insert(trade_key.clone());
            }
            if cfg.execute_orders {
                log_copy_event(
                    "real",
                    format!(
                        "orden copiada {} guardada en historial side={} outcome={} leader_price={} sim_price={} cantidad={}",
                        record.movement_id,
                        record.copy_side,
                        record.outcome,
                        record.leader_price,
                        record.simulated_copy_price,
                        record.quantity
                    ),
                );
            } else {
                log_copy_event(
                    "real",
                    format!(
                        "orden registrada (dry-run) {} side={} outcome={} leader_price={} sim_price={} cantidad={}",
                        record.movement_id,
                        record.copy_side,
                        record.outcome,
                        record.leader_price,
                        record.simulated_copy_price,
                        record.quantity
                    ),
                );
            }
        }

        log_copy_event(
            "core",
            format!("ciclo monitor #{loop_tick} finalizado; esperando {poll_ms}ms"),
        );
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

async fn fetch_trades_paginated(
    data_client: &polymarket_client_sdk::data::Client,
    user: alloy::primitives::Address,
    page_size: i32,
    max_pages: i32,
    log_scope: &str,
) -> Result<Vec<polymarket_client_sdk::data::types::response::Trade>> {
    const MAX_TRADES_OFFSET: i32 = 3000;

    let mut offset = 0;
    let mut out = Vec::new();

    for _ in 0..max_pages {
        if offset > MAX_TRADES_OFFSET {
            log_copy_event(
                log_scope,
                format!(
                    "paginación trades detenida por límite de offset de API (offset={}, max={})",
                    offset, MAX_TRADES_OFFSET
                ),
            );
            break;
        }

        let req = TradesRequest::builder()
            .user(user)
            .limit(page_size)
            .map_err(|e| anyhow!("error construyendo limit de trades: {e}"))?
            .maybe_offset(Some(offset))
            .map_err(|e| anyhow!("error construyendo offset de trades: {e}"))?
            .build();

        let batch = tokio::time::timeout(Duration::from_secs(8), data_client.trades(&req))
            .await
            .map_err(|_| anyhow!("timeout consultando trades"))??;

        let count = batch.len();
        out.extend(batch);
        if count < page_size as usize {
            break;
        }

        if offset + page_size > MAX_TRADES_OFFSET {
            log_copy_event(
                log_scope,
                format!(
                    "paginación trades alcanzó último offset permitido (offset={}, page_size={}, max={})",
                    offset, page_size, MAX_TRADES_OFFSET
                ),
            );
            break;
        }

        tokio::time::sleep(Duration::from_millis(120)).await;
        offset += page_size;
    }

    log_copy_event(
        log_scope,
        format!("paginación trades completada: {} movimientos", out.len()),
    );

    Ok(out)
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

    let leader = match crate::commands::parse_address(&cfg.leader) {
        Ok(addr) => addr,
        Err(e) => {
            let mut runtime = app.runtime.lock().await;
            runtime.warning = Some(format!("Leader inválido en simulación: {e}"));
            log_copy_event("sim", format!("error parseando leader: {e}"));
            return Ok(());
        }
    };
    let value_req = ValueRequest::builder().user(leader).build();
    let leader_value = data_client
        .value(&value_req)
        .await
        .ok()
        .and_then(|v| v.first().map(|x| x.value))
        .unwrap_or(Decimal::ONE);

    let should_sync_closed = {
        let runtime = app.runtime.lock().await;
        closed_sync_due(runtime.next_closed_sync_sim_at_ms) && !runtime.closed_sync_sim_in_flight
    };

    if should_sync_closed {
        {
            let mut runtime = app.runtime.lock().await;
            runtime.closed_sync_sim_in_flight = true;
        }
        let app_bg = app.clone();
        tokio::spawn(async move {
            run_closed_sync_task(app_bg, leader, StorageMode::Simulation, "sim").await;
        });
    }

    let should_sync_market = {
        let runtime = app.runtime.lock().await;
        closed_sync_due(runtime.next_market_sync_sim_at_ms) && !runtime.market_sync_sim_in_flight
    };

    if should_sync_market {
        {
            let mut runtime = app.runtime.lock().await;
            runtime.market_sync_sim_in_flight = true;
        }
        let app_bg = app.clone();
        tokio::spawn(async move {
            run_market_closed_sync_task(app_bg, leader, StorageMode::Simulation, "sim").await;
        });
    }

    log_copy_event(
        "sim",
        format!("consultando ultimos movimientos de la cuenta a copiar ({leader})"),
    );
    let bootstrap_needed = {
        let runtime = app.runtime.lock().await;
        !runtime.simulation_bootstrap_done
            && closed_sync_due(runtime.simulation_bootstrap_next_retry_at_ms)
    };

    let trades = if bootstrap_needed {
        log_copy_event(
            "sim",
            "bootstrap simulación: descargando historial acotado de trades para evitar throttle",
        );
        match fetch_trades_paginated(data_client, leader, 200, 6, "sim").await {
            Ok(mut t) => {
                t.sort_by_key(|x| x.timestamp);
                let mut runtime = app.runtime.lock().await;
                runtime.simulation_bootstrap_done = true;
                t
            }
            Err(e) => {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(format!(
                    "Error bootstrap simulación consultando trades: {e}"
                ));
                runtime.simulation_bootstrap_next_retry_at_ms =
                    now_ms() + i64::try_from(SIM_BOOTSTRAP_RETRY_MS).unwrap_or(300_000);
                log_copy_event(
                    "sim",
                    format!(
                        "error bootstrap consultando trades: {e}; próximo reintento en ~{}s",
                        SIM_BOOTSTRAP_RETRY_MS / 1000
                    ),
                );
                Vec::new()
            }
        }
    } else {
        let trades_req = TradesRequest::builder().user(leader).limit(20)?.build();
        match tokio::time::timeout(Duration::from_secs(15), data_client.trades(&trades_req)).await {
            Ok(Ok(mut trades)) => {
                log_copy_event(
                    "sim",
                    format!("consulta trades completada: {} movimientos", trades.len()),
                );
                trades.sort_by_key(|x| x.timestamp);
                trades
            }
            Ok(Err(e)) => {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(format!("Error simulación consultando trades: {e}"));
                log_copy_event("sim", format!("error consultando trades recientes: {e}"));
                Vec::new()
            }
            Err(_) => {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some("Timeout simulación consultando trades".to_string());
                log_copy_event("sim", "timeout consultando ultimos movimientos (15s)");
                Vec::new()
            }
        }
    };

    let prime_only = {
        let mut runtime = app.runtime.lock().await;
        if runtime.last_seen_trade_keys_sim.is_empty() {
            for t in &trades {
                runtime.last_seen_trade_keys_sim.insert(trade_event_key(t));
            }
            true
        } else {
            false
        }
    };

    if prime_only {
        log_copy_event(
            "sim",
            format!(
                "primer barrido sim: {} trades marcados como vistos (sin copiar histórico)",
                trades.len()
            ),
        );
        return Ok(());
    }

    for t in trades {
        let tx_hash = t.transaction_hash.to_string();
        let trade_key = trade_event_key(&t);
        let is_sell = t.side.to_string().eq_ignore_ascii_case("sell");
        {
            let mut runtime = app.runtime.lock().await;
            if runtime.last_seen_trade_keys_sim.contains(&trade_key) {
                continue;
            }
            if !is_sell {
                runtime.last_seen_trade_keys_sim.insert(trade_key.clone());
            }
        }

        let mut state = load_state()?;
        let movement_id = format!("sim-{trade_key}");
        if state.movements.iter().any(|m| m.movement_id == movement_id) {
            continue;
        }

        if is_sell {
            let settled_from_sell =
                settle_open_buys_from_sell_trade(&mut state, &t.slug, &t.outcome, t.price);
            if !settled_from_sell.is_empty() {
                save_state(&state)?;
                for movement in settled_from_sell {
                    settle_db_movement(
                        StorageMode::Simulation,
                        &movement.movement_id,
                        movement.pnl,
                    )?;
                    if let Err(e) = append_settlement_log(StorageMode::Simulation, &movement) {
                        log_copy_event("sim", format!("error escribiendo log de settlement: {e}"));
                    }
                    log_copy_event(
                        "sim",
                        format!(
                            "sell líder (sim) detectado: cerrada {} (mercado={}, outcome={}) pnl={} por precio de salida {}",
                            movement.movement_id,
                            movement.market,
                            movement.outcome,
                            movement.pnl,
                            t.price
                        ),
                    );
                }
                let mut runtime = app.runtime.lock().await;
                runtime.last_seen_trade_keys_sim.insert(trade_key.clone());
                continue;
            }
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

        if t.side.to_string().eq_ignore_ascii_case("sell") {
            let required_sell_shares = copied_shares_from_notional(plan.capped_size, t.price);
            if !has_enough_inventory_for_sell(&state, &t.slug, &t.outcome, required_sell_shares) {
                log_copy_event(
                    "sim",
                    format!(
                        "simulacion sell {} ({}) descartada: no hay buy abierto conciliable (outcome={}, required_shares={})",
                        t.slug, tx_hash, t.outcome, required_sell_shares
                    ),
                );
                continue;
            }

            log_copy_event(
                "sim",
                format!(
                    "simulacion sell {} ({}) descartada: no se pudo conciliar cierre inmediato; evitando SELL abierto",
                    t.slug, tx_hash
                ),
            );
            continue;
        }

        let fee_impact = trading_fee_impact_for_movement(&t.slug, plan.capped_size);
        if let Some(impact) = fee_impact
            && impact.max_net_profit_usd <= Decimal::ZERO
        {
            log_copy_event(
                "sim",
                format!(
                    "simulacion descartada por fees {} ({}) ({} bps): profit_max_neto={} (gross_max={} fee_entry={} fees_rt={})",
                    t.slug,
                    tx_hash,
                    impact.fee_bps,
                    impact.max_net_profit_usd,
                    impact.max_gross_profit_usd,
                    impact.entry_fee_usd,
                    impact.round_trip_fee_usd,
                ),
            );
            continue;
        }

        log_copy_event(
            "sim",
            format!(
                "nueva apuesta detectada {} ({}) side={} outcome={} leader_usd={} leader_price={} cantidad={} simulacion_plan={} sim_price={} motivo={}",
                t.slug,
                tx_hash,
                t.side,
                t.outcome,
                t.size * t.price,
                t.price,
                t.size,
                plan.capped_size,
                t.price,
                plan.reason
            ),
        );

        let (estimated_sim_price, has_full_liquidity) =
            match estimate_simulated_copy_price_from_book(clob_client, &t, plan.capped_size).await {
                Ok(v) => v,
                Err(e) => {
                    let mut runtime = app.runtime.lock().await;
                    runtime.warning = Some(format!("Error chequeando liquidez simulación: {e}"));
                    log_copy_event(
                        "sim",
                        format!("error chequeando liquidez {} ({}): {e}", t.slug, tx_hash),
                    );
                    continue;
                }
            };
        log_copy_event(
            "sim",
            format!(
                "chequeo liquidez {} ({}): {}",
                t.slug,
                tx_hash,
                if has_full_liquidity {
                    "SI"
                } else if estimated_sim_price.is_some() {
                    "PARCIAL"
                } else {
                    "NO"
                }
            ),
        );
        if estimated_sim_price.is_none() {
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

        if !has_full_liquidity {
            let mut runtime = app.runtime.lock().await;
            runtime.warning = Some(format!(
                "Simulación: liquidez parcial en {} ({}), estimación de precio con fill parcial",
                t.slug, tx_hash
            ));
        }

        let record = MovementRecord {
            movement_id,
            market: t.slug,
            timestamp: Utc::now().to_rfc3339(),
            leader_value: t.size * t.price,
            leader_price: t.price,
            copied_value: plan.capped_size,
            simulated_copy_price: estimated_sim_price.unwrap_or(Decimal::ZERO),
            quantity: t.size,
            copy_side: t.side.to_string(),
            outcome: t.outcome.clone(),
            resolved_outcome: String::new(),
            diff_pct: Decimal::ZERO,
            estimated_total_fee_usd: fee_impact
                .map(|x| x.round_trip_fee_usd)
                .unwrap_or(Decimal::ZERO),
            settled: false,
            pnl: Decimal::ZERO,
        };
        let mut updated = state;
        updated.movements.push(record.clone());
        save_state(&updated)?;
        append_db_movement(StorageMode::Simulation, &record)?;
        if is_sell {
            let mut runtime = app.runtime.lock().await;
            runtime.last_seen_trade_keys_sim.insert(trade_key.clone());
        }
        log_copy_event(
            "sim",
            format!(
                "apuesta simulada registrada {} side={} outcome={} leader_price={} sim_price={} cantidad={}",
                record.movement_id,
                record.copy_side,
                record.outcome,
                record.leader_price,
                record.simulated_copy_price,
                record.quantity
            ),
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

fn unsettled_market_slugs(state: &CopyState) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in state.movements.iter().filter(|m| !m.settled) {
        let normalized = normalize_market_slug(&m.market);
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    out
}

fn apply_settlements_from_closed_positions(
    mode: StorageMode,
    log_scope: &'static str,
    closed_positions: &[polymarket_client_sdk::data::types::response::ClosedPosition],
) -> Result<()> {
    let closed_keys = closed_slug_keys(closed_positions);
    if let Some((oldest_movement_id, oldest_market)) = oldest_unsettled_from_db(mode)?
        && is_market_closed(&closed_keys, &oldest_market)
    {
        log_copy_event(
            log_scope,
            format!(
                "cierre detectado para la apuesta abierta más antigua {} ({})",
                oldest_movement_id, oldest_market
            ),
        );
    }

    let mut state = load_state()?;
    let settled = settle_open_movements_from_closed_positions(&mut state, closed_positions);
    if !settled.is_empty() {
        save_state(&state)?;
        for movement in settled {
            log_copy_event(
                log_scope,
                format!(
                    "resuelta {} (mercado={}) pnl={} -> fondos liberados",
                    movement.movement_id, movement.market, movement.pnl
                ),
            );
            settle_db_movement(mode, &movement.movement_id, movement.pnl)?;
            if let Err(e) = append_settlement_log(mode, &movement) {
                log_copy_event(
                    log_scope,
                    format!("error escribiendo log de settlement: {e}"),
                );
            }
        }
    }

    Ok(())
}

fn settle_open_buys_from_resolved_markets(
    state: &mut CopyState,
    resolved_outcomes: &HashMap<String, String>,
) -> Vec<MovementRecord> {
    let mut settled = Vec::new();

    for movement in state.movements.iter_mut().filter(|m| !m.settled) {
        if !movement.copy_side.eq_ignore_ascii_case("buy") {
            continue;
        }

        let normalized_market = normalize_market_slug(&movement.market);
        let Some(resolved_outcome) = resolved_outcomes.get(&normalized_market) else {
            continue;
        };

        let shares = movement_copied_shares(movement);
        if shares <= Decimal::ZERO {
            continue;
        }

        let payout_per_share = if movement.outcome == *resolved_outcome {
            Decimal::ONE
        } else {
            Decimal::ZERO
        };
        movement.pnl = (shares * payout_per_share) - movement.copied_value;
        movement.copy_side = "sell".to_string();
        movement.resolved_outcome = resolved_outcome.clone();
        movement.settled = true;
        settled.push(movement.clone());
    }

    settled
}

fn apply_settlements_from_resolved_markets(
    mode: StorageMode,
    log_scope: &'static str,
    resolved_outcomes: &HashMap<String, String>,
) -> Result<()> {
    if resolved_outcomes.is_empty() {
        return Ok(());
    }

    let mut state = load_state()?;
    let settled = settle_open_buys_from_resolved_markets(&mut state, resolved_outcomes);
    if settled.is_empty() {
        return Ok(());
    }

    save_state(&state)?;
    for movement in settled {
        settle_db_movement(mode, &movement.movement_id, movement.pnl)?;
        if let Err(e) = append_settlement_log(mode, &movement) {
            log_copy_event(
                log_scope,
                format!("error escribiendo log de settlement: {e}"),
            );
        }
        log_copy_event(
            log_scope,
            format!(
                "resolución de mercado cerró {} (mercado={}, ganador={}, outcome={}) pnl={}",
                movement.movement_id,
                movement.market,
                movement.resolved_outcome,
                movement.outcome,
                movement.pnl
            ),
        );
    }

    Ok(())
}

fn resolved_outcome_from_market(
    market: &polymarket_client_sdk::gamma::types::response::Market,
) -> Option<String> {
    let outcomes = market.outcomes.as_ref()?;
    let prices = market.outcome_prices.as_ref()?;
    if outcomes.len() != prices.len() {
        return None;
    }

    let resolved_price_threshold = Decimal::from_str_exact("0.999").unwrap_or(Decimal::ONE);
    for (outcome, price) in outcomes.iter().zip(prices.iter()) {
        if *price >= resolved_price_threshold {
            return Some(outcome.clone());
        }
    }

    None
}

async fn fetch_closed_markets_from_gamma(
    slugs: &[String],
    log_scope: &str,
) -> Result<(HashSet<String>, HashMap<String, String>)> {
    const CHUNK_SIZE: usize = 25;

    let gamma_client = polymarket_client_sdk::gamma::Client::default();
    let mut closed = HashSet::new();
    let mut resolved_outcomes = HashMap::new();

    for chunk in slugs.chunks(CHUNK_SIZE) {
        let req = MarketsRequest::builder()
            .slug(chunk.to_vec())
            .closed(true)
            .build();
        let markets = tokio::time::timeout(Duration::from_secs(15), gamma_client.markets(&req))
            .await
            .map_err(|_| anyhow!("timeout consultando mercados cerrados"))??;

        for market in markets {
            if market.closed.unwrap_or(false)
                && let Some(slug) = market.slug.as_ref()
            {
                let normalized = normalize_market_slug(slug);
                closed.insert(normalized.clone());
                if let Some(resolved_outcome) = resolved_outcome_from_market(&market) {
                    resolved_outcomes.insert(normalized, resolved_outcome);
                }
            }
        }
    }

    log_copy_event(
        log_scope,
        format!(
            "sync mercado: slugs consultados={}, cerrados_detectados={}, resolucion_detectada={}",
            slugs.len(),
            closed.len(),
            resolved_outcomes.len()
        ),
    );

    Ok((closed, resolved_outcomes))
}

async fn run_market_closed_sync_task(
    app: UiAppState,
    user: alloy::primitives::Address,
    mode: StorageMode,
    log_scope: &'static str,
) {
    let result: Result<()> = async {
        let state = load_state()?;
        let unsettled_slugs = unsettled_market_slugs(&state);
        if unsettled_slugs.is_empty() {
            return Ok(());
        }

        let (closed_market_slugs, resolved_outcomes) =
            fetch_closed_markets_from_gamma(&unsettled_slugs, log_scope).await?;
        if closed_market_slugs.is_empty() {
            return Ok(());
        }

        let should_reconcile_user = unsettled_slugs
            .iter()
            .any(|s| closed_market_slugs.contains(s));
        if !should_reconcile_user {
            return Ok(());
        }

        log_copy_event(
            log_scope,
            format!(
                "mercado reporta cierres para {} slugs; forzando conciliación por cuenta {}",
                closed_market_slugs.len(),
                user
            ),
        );

        let data_client = polymarket_client_sdk::data::Client::default();
        let closed_positions =
            fetch_closed_positions_paginated(&data_client, user, log_scope).await?;
        apply_settlements_from_closed_positions(mode, log_scope, &closed_positions)?;
        apply_settlements_from_resolved_markets(mode, log_scope, &resolved_outcomes)
    }
    .await;

    let mut runtime = app.runtime.lock().await;
    match result {
        Ok(_) => schedule_market_sync_success(&mut runtime, mode),
        Err(e) => {
            runtime.warning = Some(match mode {
                StorageMode::Real => format!("Error consultando cierre de mercados: {e}"),
                StorageMode::Simulation => format!("Error simulación cierre de mercados: {e}"),
            });
            schedule_market_sync_backoff(&mut runtime, mode);
        }
    }
    match mode {
        StorageMode::Real => runtime.market_sync_real_in_flight = false,
        StorageMode::Simulation => runtime.market_sync_sim_in_flight = false,
    }
}

fn settle_open_buys_from_activities(
    state: &mut CopyState,
    activities: &[polymarket_client_sdk::data::types::response::Activity],
) -> Vec<MovementRecord> {
    let mut settled = Vec::new();

    for a in activities {
        let is_close_activity =
            matches!(a.activity_type, ActivityType::Merge | ActivityType::Redeem);
        if !is_close_activity {
            continue;
        }

        let Some(slug) = a.slug.as_ref() else {
            continue;
        };
        let normalized_slug = normalize_market_slug(slug);
        let activity_outcome = a.outcome.as_deref().unwrap_or("");

        let mut exit_price = a.price.unwrap_or(Decimal::ZERO);
        if exit_price <= Decimal::ZERO && a.size > Decimal::ZERO && a.usdc_size > Decimal::ZERO {
            exit_price = a.usdc_size / a.size;
        }

        for movement in state.movements.iter_mut().filter(|m| !m.settled) {
            if !movement.copy_side.eq_ignore_ascii_case("buy") {
                continue;
            }
            let movement_norm = normalize_market_slug(&movement.market);
            if movement.market != *slug && movement_norm != normalized_slug {
                continue;
            }
            if !activity_outcome.is_empty() && movement.outcome != activity_outcome {
                continue;
            }

            let entry_price = if movement.simulated_copy_price > Decimal::ZERO {
                movement.simulated_copy_price
            } else {
                movement.leader_price
            };

            if exit_price > Decimal::ZERO && entry_price > Decimal::ZERO {
                let roi = (exit_price - entry_price) / entry_price;
                movement.pnl = movement.copied_value * roi;
            }
            movement.copy_side = "sell".to_string();
            if !activity_outcome.is_empty() {
                movement.resolved_outcome = activity_outcome.to_string();
            }
            movement.settled = true;
            settled.push(movement.clone());
        }
    }

    settled
}

fn apply_settlements_from_activity(
    mode: StorageMode,
    log_scope: &'static str,
    activities: &[polymarket_client_sdk::data::types::response::Activity],
) -> Result<()> {
    let mut state = load_state()?;
    let settled = settle_open_buys_from_activities(&mut state, activities);
    if settled.is_empty() {
        return Ok(());
    }

    save_state(&state)?;
    for movement in settled {
        settle_db_movement(mode, &movement.movement_id, movement.pnl)?;
        if let Err(e) = append_settlement_log(mode, &movement) {
            log_copy_event(
                log_scope,
                format!("error escribiendo log de settlement: {e}"),
            );
        }
        log_copy_event(
            log_scope,
            format!(
                "actividad on-chain cerró {} (mercado={}, outcome={}) pnl={}",
                movement.movement_id, movement.market, movement.outcome, movement.pnl
            ),
        );
    }

    Ok(())
}

async fn fetch_activity_paginated(
    data_client: &polymarket_client_sdk::data::Client,
    user: alloy::primitives::Address,
    log_scope: &str,
) -> Result<Vec<polymarket_client_sdk::data::types::response::Activity>> {
    const PAGE_SIZE: i32 = 500;
    const MAX_PAGES: i32 = 20;

    let mut offset = 0;
    let mut out = Vec::new();
    for _ in 0..MAX_PAGES {
        let req = ActivityRequest::builder()
            .user(user)
            .limit(PAGE_SIZE)
            .map_err(|e| anyhow!("error construyendo limit de activity: {e}"))?
            .activity_types(vec![ActivityType::Merge, ActivityType::Redeem])
            .maybe_offset(Some(offset))
            .map_err(|e| anyhow!("error construyendo offset de activity: {e}"))?
            .build();

        let batch = tokio::time::timeout(Duration::from_secs(15), data_client.activity(&req))
            .await
            .map_err(|_| anyhow!("timeout consultando activity"))??;

        let count = batch.len();
        out.extend(batch);
        if count < PAGE_SIZE as usize {
            break;
        }
        offset += PAGE_SIZE;
    }

    log_copy_event(
        log_scope,
        format!("consulta activity merge/redeem completada: {}", out.len()),
    );
    Ok(out)
}

async fn run_closed_sync_task(
    app: UiAppState,
    user: alloy::primitives::Address,
    mode: StorageMode,
    log_scope: &'static str,
) {
    log_copy_event(
        log_scope,
        format!("consultando cierres/resoluciones de la cuenta a copiar ({user})"),
    );

    let data_client = polymarket_client_sdk::data::Client::default();
    let result = fetch_closed_positions_paginated(&data_client, user, log_scope).await;

    match result {
        Ok(closed_positions) => {
            if closed_positions.is_empty() {
                let mut runtime = app.runtime.lock().await;
                runtime.warning = Some(match mode {
                    StorageMode::Real => {
                        "No se pudieron obtener cierres recientes (paginación vacía o error)"
                            .to_string()
                    }
                    StorageMode::Simulation => {
                        "Simulación: no se pudieron obtener cierres recientes".to_string()
                    }
                });
                schedule_closed_sync_backoff(&mut runtime, mode);
            } else {
                let settle_result =
                    apply_settlements_from_closed_positions(mode, log_scope, &closed_positions);

                if settle_result.is_ok() {
                    if let Ok(activities) =
                        fetch_activity_paginated(&data_client, user, log_scope).await
                    {
                        let _ = apply_settlements_from_activity(mode, log_scope, &activities);
                    }
                }

                let mut runtime = app.runtime.lock().await;
                match settle_result {
                    Ok(_) => schedule_closed_sync_success(&mut runtime, mode),
                    Err(e) => {
                        runtime.warning = Some(format!("Error conciliando cierres: {e}"));
                        schedule_closed_sync_backoff(&mut runtime, mode);
                    }
                }
            }
        }
        Err(e) => {
            let mut runtime = app.runtime.lock().await;
            runtime.warning = Some(match mode {
                StorageMode::Real => format!("Error consultando posiciones cerradas: {e}"),
                StorageMode::Simulation => format!("Error simulación consultando cerradas: {e}"),
            });
            schedule_closed_sync_backoff(&mut runtime, mode);
        }
    }

    let mut runtime = app.runtime.lock().await;
    match mode {
        StorageMode::Real => runtime.closed_sync_real_in_flight = false,
        StorageMode::Simulation => runtime.closed_sync_sim_in_flight = false,
    }
}

async fn fetch_closed_positions_paginated(
    data_client: &polymarket_client_sdk::data::Client,
    user: alloy::primitives::Address,
    log_scope: &str,
) -> Result<Vec<polymarket_client_sdk::data::types::response::ClosedPosition>> {
    const PAGE_SIZE: i32 = 50;
    const MAX_PAGES: i32 = 40;

    let mut offset = 0;
    let mut out = Vec::new();

    for page in 0..MAX_PAGES {
        let req = match ClosedPositionsRequest::builder()
            .user(user)
            .limit(PAGE_SIZE)
            .and_then(|b| b.maybe_offset(Some(offset)))
        {
            Ok(b) => b.build(),
            Err(e) => {
                log_copy_event(
                    log_scope,
                    format!("error construyendo request de cierres: {e}"),
                );
                return Err(anyhow!("error construyendo request de cierres: {e}"));
            }
        };

        let batch =
            match tokio::time::timeout(Duration::from_secs(15), data_client.closed_positions(&req))
                .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    log_copy_event(
                        log_scope,
                        format!(
                            "error consultando cierres paginados (page={}, offset={}): {}",
                            page, offset, e
                        ),
                    );
                    return Err(anyhow!(
                        "error consultando cierres paginados (page={}, offset={}): {}",
                        page,
                        offset,
                        e
                    ));
                }
                Err(_) => {
                    log_copy_event(
                        log_scope,
                        format!(
                            "timeout consultando cierres paginados (page={}, offset={})",
                            page, offset
                        ),
                    );
                    return Err(anyhow!(
                        "timeout consultando cierres paginados (page={}, offset={})",
                        page,
                        offset
                    ));
                }
            };

        let batch_len = batch.len();
        out.extend(batch);
        if batch_len < PAGE_SIZE as usize {
            break;
        }

        offset += PAGE_SIZE;
    }

    log_copy_event(
        log_scope,
        format!(
            "consulta de cierres paginada completada: {} posiciones",
            out.len()
        ),
    );

    Ok(out)
}

async fn estimate_simulated_copy_price_from_book(
    clob_client: &polymarket_client_sdk::clob::Client,
    trade: &polymarket_client_sdk::data::types::response::Trade,
    copied_value_usd: Decimal,
) -> Result<(Option<Decimal>, bool)> {
    let req = OrderBookSummaryRequest::builder()
        .token_id(trade.asset)
        .build();
    let book = clob_client.order_book(&req).await?;

    if trade.side.to_string().eq_ignore_ascii_case("buy") {
        let mut remaining_usdc = copied_value_usd;
        let mut filled_usdc = Decimal::ZERO;
        let mut filled_shares = Decimal::ZERO;
        for ask in &book.asks {
            if remaining_usdc <= Decimal::ZERO {
                break;
            }
            let level_notional = ask.size * ask.price;
            let take_notional = if level_notional >= remaining_usdc {
                remaining_usdc
            } else {
                level_notional
            };
            if ask.price > Decimal::ZERO {
                filled_shares += take_notional / ask.price;
            }
            filled_usdc += take_notional;
            remaining_usdc -= take_notional;
        }
        if filled_shares <= Decimal::ZERO {
            return Ok((None, false));
        }
        Ok((
            Some(filled_usdc / filled_shares),
            remaining_usdc <= Decimal::ZERO,
        ))
    } else {
        if trade.price <= Decimal::ZERO {
            return Ok((None, false));
        }
        let mut remaining_shares = copied_value_usd / trade.price;
        let mut sold_shares = Decimal::ZERO;
        let mut received_usdc = Decimal::ZERO;
        for bid in &book.bids {
            if remaining_shares <= Decimal::ZERO {
                break;
            }
            let take_shares = if bid.size >= remaining_shares {
                remaining_shares
            } else {
                bid.size
            };
            sold_shares += take_shares;
            received_usdc += take_shares * bid.price;
            remaining_shares -= take_shares;
        }
        if sold_shares <= Decimal::ZERO {
            return Ok((None, false));
        }
        Ok((
            Some(received_usdc / sold_shares),
            remaining_shares <= Decimal::ZERO,
        ))
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
        && ms < min_poll_ms(cfg.realtime_mode, cfg.simulation_mode)
    {
        bail!("poll-interval-ms too low for selected mode");
    }
    Ok(())
}

fn copied_shares_from_notional(notional_usd: Decimal, price: Decimal) -> Decimal {
    if notional_usd <= Decimal::ZERO || price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    notional_usd / price
}

fn trade_event_key(trade: &polymarket_client_sdk::data::types::response::Trade) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}",
        trade.transaction_hash,
        trade.asset,
        trade.side,
        trade.outcome,
        trade.slug,
        trade.timestamp,
        trade.size,
        trade.price,
    )
}

fn movement_copied_shares(m: &MovementRecord) -> Decimal {
    let px = if m.simulated_copy_price > Decimal::ZERO {
        m.simulated_copy_price
    } else {
        m.leader_price
    };
    copied_shares_from_notional(m.copied_value, px)
}

fn settle_open_buys_from_sell_trade(
    state: &mut CopyState,
    market: &str,
    outcome: &str,
    sell_price: Decimal,
) -> Vec<MovementRecord> {
    if sell_price <= Decimal::ZERO {
        return Vec::new();
    }

    let normalized_market = normalize_market_slug(market);
    let mut settled = Vec::new();

    for movement in state.movements.iter_mut().filter(|m| !m.settled) {
        if !movement.copy_side.eq_ignore_ascii_case("buy") {
            continue;
        }
        if movement.outcome != outcome {
            continue;
        }
        let movement_market_norm = normalize_market_slug(&movement.market);
        if movement.market != market && movement_market_norm != normalized_market {
            continue;
        }

        let entry_price = if movement.simulated_copy_price > Decimal::ZERO {
            movement.simulated_copy_price
        } else {
            movement.leader_price
        };
        if entry_price <= Decimal::ZERO {
            continue;
        }

        let roi = (sell_price - entry_price) / entry_price;
        movement.pnl = movement.copied_value * roi;
        movement.copy_side = "sell".to_string();
        movement.resolved_outcome = outcome.to_string();
        movement.settled = true;
        settled.push(movement.clone());
    }

    settled
}

fn has_enough_inventory_for_sell(
    state: &CopyState,
    market: &str,
    outcome: &str,
    required_sell_shares: Decimal,
) -> bool {
    if required_sell_shares <= Decimal::ZERO {
        return false;
    }

    let mut net_long_shares = Decimal::ZERO;
    for movement in state.movements.iter().filter(|m| !m.settled) {
        if movement.market != market || movement.outcome != outcome {
            continue;
        }
        let shares = movement_copied_shares(movement);
        if shares <= Decimal::ZERO {
            continue;
        }
        if movement.copy_side.eq_ignore_ascii_case("buy") {
            net_long_shares += shares;
        } else if movement.copy_side.eq_ignore_ascii_case("sell") {
            net_long_shares -= shares;
        }
    }

    net_long_shares >= required_sell_shares
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
    let settled_pnl_after_fees: Decimal = state
        .movements
        .iter()
        .filter(|m| m.settled)
        .map(|m| m.pnl - m.estimated_total_fee_usd)
        .sum();
    let effective_funds = (cfg.allocated_funds + settled_pnl_after_fees).max(Decimal::ZERO);

    let ratio = effective_funds / leader_positions_value;
    let proportional = leader_movement_value * ratio;

    let safe_max_trade_pct = cfg.max_trade_pct.min(Decimal::from(100));
    let safe_max_total_exposure_pct = cfg.max_total_exposure_pct.min(Decimal::from(100));

    let max_trade = effective_funds * (safe_max_trade_pct / Decimal::from(100));
    let max_total_exposure = effective_funds * (safe_max_total_exposure_pct / Decimal::from(100));
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

fn normalize_market_slug(slug: &str) -> String {
    let Some((prefix, suffix)) = slug.rsplit_once('-') else {
        return slug.to_string();
    };
    if suffix.len() >= 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
        prefix.to_string()
    } else {
        slug.to_string()
    }
}

fn closed_slug_keys(
    closed_positions: &[polymarket_client_sdk::data::types::response::ClosedPosition],
) -> HashSet<String> {
    let mut keys = HashSet::new();
    for closed in closed_positions {
        keys.insert(closed.slug.clone());
        keys.insert(normalize_market_slug(&closed.slug));
    }
    keys
}

fn calculate_settlement_pnl_from_invested(
    invested_usd: Decimal,
    total_bought_usd: Decimal,
    realized_pnl_usd: Decimal,
) -> Decimal {
    if invested_usd <= Decimal::ZERO || total_bought_usd <= Decimal::ZERO {
        return Decimal::ZERO;
    }

    invested_usd * (realized_pnl_usd / total_bought_usd)
}

fn oldest_unsettled_db_row(rows: &[DbRow]) -> Option<&DbRow> {
    rows.iter().filter(|r| !r.settled).min_by_key(|r| r.id)
}

fn oldest_unsettled_from_db(mode: StorageMode) -> Result<Option<(String, String)>> {
    let rows = read_db_rows(mode)?;
    Ok(oldest_unsettled_db_row(&rows).map(|r| (r.movement_id.clone(), r.market.clone())))
}

fn is_market_closed(closed_keys: &HashSet<String>, market: &str) -> bool {
    let normalized_market = normalize_market_slug(market);
    closed_keys.contains(market) || closed_keys.contains(normalized_market.as_str())
}

fn movement_timestamp_epoch_seconds(ts: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|dt| dt.timestamp())
}

fn settle_open_movements_from_closed_positions(
    state: &mut CopyState,
    closed_positions: &[polymarket_client_sdk::data::types::response::ClosedPosition],
) -> Vec<MovementRecord> {
    type ClosedEntry = (i64, Decimal, Decimal, String);
    let mut by_market_outcome: HashMap<(String, String), VecDeque<ClosedEntry>> = HashMap::new();
    let mut closed_sorted = closed_positions.to_vec();
    closed_sorted.sort_by_key(|c| c.timestamp);

    for closed in closed_sorted {
        if closed.total_bought <= Decimal::ZERO {
            continue;
        }
        let realized_pnl = closed.realized_pnl;
        let total_bought = closed.total_bought;
        let normalized = normalize_market_slug(&closed.slug);
        let key_exact = (closed.slug.clone(), closed.outcome.clone());
        by_market_outcome.entry(key_exact).or_default().push_back((
            closed.timestamp,
            total_bought,
            realized_pnl,
            closed.outcome.clone(),
        ));
        if normalized != closed.slug {
            let key_normalized = (normalized, closed.outcome.clone());
            by_market_outcome
                .entry(key_normalized)
                .or_default()
                .push_back((
                    closed.timestamp,
                    total_bought,
                    realized_pnl,
                    closed.outcome.clone(),
                ));
        }
    }

    let mut settled = Vec::new();
    for movement in state.movements.iter_mut().filter(|m| !m.settled) {
        let normalized_market = normalize_market_slug(&movement.market);

        let Some(movement_ts) = movement_timestamp_epoch_seconds(&movement.timestamp) else {
            continue;
        };

        let mut pop_eligible_roi = |q: &mut VecDeque<ClosedEntry>| {
            if q.is_empty() {
                return None;
            }

            // Prefer closures with usable timestamps that are >= movement timestamp,
            // or closures with unknown timestamp (0) which we consider usable.
            if let Some(idx) = q
                .iter()
                .position(|(ts, _, _, _)| *ts == 0 || *ts >= movement_ts)
            {
                return q
                    .remove(idx)
                    .map(|(_, total_bought, realized_pnl, outcome)| {
                        (total_bought, realized_pnl, outcome)
                    });
            }

            // Fallback: some Data API responses can carry stale/legacy timestamps.
            // In that case, consume oldest closure to avoid movements stuck forever.
            q.pop_front()
                .map(|(_, total_bought, realized_pnl, outcome)| {
                    (total_bought, realized_pnl, outcome)
                })
        };

        let outcome = movement.outcome.clone();
        let key_exact = (movement.market.clone(), outcome.clone());
        let key_normalized = (normalized_market, outcome);

        let roi_and_outcome = by_market_outcome
            .get_mut(&key_exact)
            .and_then(&mut pop_eligible_roi)
            .or_else(|| {
                by_market_outcome
                    .get_mut(&key_normalized)
                    .and_then(&mut pop_eligible_roi)
            });

        let Some((total_bought, realized_pnl, resolved_outcome)) = roi_and_outcome else {
            continue;
        };

        movement.pnl = calculate_settlement_pnl_from_invested(
            movement.copied_value,
            total_bought,
            realized_pnl,
        );
        if movement.copy_side.eq_ignore_ascii_case("buy") {
            movement.copy_side = "sell".to_string();
        }
        movement.resolved_outcome = resolved_outcome;
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

fn settlement_log_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("copy_trader_settlements.log"))
}

fn append_settlement_log(mode: StorageMode, movement: &MovementRecord) -> Result<()> {
    let path = settlement_log_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let line = format!(
        "{}\tmode={}\tmovement_id={}\tmarket={}\tside={}\toutcome={}\tresolved_outcome={}\tleader_price={}\tsimulated_copy_price={}\tquantity={}\tcopied_value={}\testimated_total_fee_usd={}\tpnl={}\n",
        Utc::now().to_rfc3339(),
        match mode {
            StorageMode::Real => "real",
            StorageMode::Simulation => "sim",
        },
        movement.movement_id,
        movement.market,
        movement.copy_side,
        movement.outcome,
        movement.resolved_outcome,
        movement.leader_price,
        movement.simulated_copy_price,
        movement.quantity,
        movement.copied_value,
        movement.estimated_total_fee_usd,
        movement.pnl,
    );
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(line.as_bytes())?;
    Ok(())
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
    #[serde(default)]
    leader_price: String,
    copied_value: String,
    #[serde(default)]
    simulated_copy_price: String,
    #[serde(default)]
    quantity: String,
    #[serde(default)]
    copy_side: String,
    #[serde(default)]
    outcome: String,
    #[serde(default)]
    resolved_outcome: String,
    diff_pct: String,
    #[serde(default)]
    estimated_total_fee_usd: String,
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
        leader_price: m.leader_price.to_string(),
        copied_value: m.copied_value.to_string(),
        simulated_copy_price: m.simulated_copy_price.to_string(),
        quantity: m.quantity.to_string(),
        copy_side: m.copy_side.clone(),
        outcome: m.outcome.clone(),
        resolved_outcome: m.resolved_outcome.clone(),
        diff_pct: m.diff_pct.to_string(),
        estimated_total_fee_usd: m.estimated_total_fee_usd.to_string(),
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
            leader_price: Decimal::from_str_exact(&r.leader_price).unwrap_or(Decimal::ZERO),
            copied_value: Decimal::from_str_exact(&r.copied_value).unwrap_or(Decimal::ZERO),
            simulated_copy_price: Decimal::from_str_exact(&r.simulated_copy_price)
                .unwrap_or(Decimal::ZERO),
            quantity: Decimal::from_str_exact(&r.quantity).unwrap_or(Decimal::ZERO),
            copy_side: r.copy_side,
            outcome: r.outcome,
            resolved_outcome: r.resolved_outcome,
            diff_pct: Decimal::from_str_exact(&r.diff_pct).unwrap_or(Decimal::ZERO),
            estimated_total_fee_usd: Decimal::from_str_exact(&r.estimated_total_fee_usd)
                .unwrap_or(Decimal::ZERO),
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
            leader_price: r.leader_price,
            copied_value: r.copied_value,
            simulated_copy_price: r.simulated_copy_price,
            quantity: r.quantity,
            copy_side: r.copy_side,
            outcome: r.outcome,
            resolved_outcome: r.resolved_outcome,
            diff_pct: r.diff_pct,
            estimated_total_fee_usd: r.estimated_total_fee_usd,
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
        let day = m
            .timestamp
            .get(0..13)
            .map(|v| format!("{}:00", v.replace('T', " ")))
            .unwrap_or_else(|| "unknown".to_string());
        let net_pnl = m.pnl - m.estimated_total_fee_usd;
        by_day
            .entry(day)
            .and_modify(|x| *x += net_pnl)
            .or_insert(net_pnl);
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
                leader_price: Decimal::ZERO,
                copied_value: d("550"),
                simulated_copy_price: Decimal::ZERO,
                quantity: Decimal::ZERO,
                copy_side: "unknown".into(),
                outcome: String::new(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };
        let p = compute_plan(&cfg, &state, d("1000"), d("100")).unwrap();
        assert_eq!(p.capped_size, d("50"));
        assert_eq!(p.available_funds, d("50"));
    }

    #[test]
    fn copy_log_filter_skips_high_frequency_query_messages() {
        assert!(!should_persist_copy_log_message(
            "consultando ultimos movimientos de la cuenta a copiar"
        ));
        assert!(!should_persist_copy_log_message(
            "consulta trades completada: 20 movimientos"
        ));
        assert!(!should_persist_copy_log_message(
            "consulta de cierres completada: 3 posiciones"
        ));
        assert!(!should_persist_copy_log_message(
            "timeout consultando cierres (15s)"
        ));
        assert!(!should_persist_copy_log_message(
            "tick simulacion (poll=50ms)"
        ));
        assert!(!should_persist_copy_log_message(
            "ciclo monitor #12 finalizado; esperando 50ms"
        ));

        assert!(should_persist_copy_log_message(
            "nueva apuesta detectada eth-updown-5m (...)"
        ));
        assert!(should_persist_copy_log_message(
            "sell ... descartado: no hay inventario"
        ));
        assert!(should_persist_copy_log_message(
            "resuelta simulacion sim-0xabc pnl=1.2"
        ));
    }

    #[test]
    fn sell_requires_previous_open_buy_inventory() {
        let state = CopyState {
            movements: vec![
                MovementRecord {
                    movement_id: "b1".into(),
                    market: "eth-updown-5m-1772281500".into(),
                    timestamp: "2026-02-28T12:00:00Z".into(),
                    leader_value: d("10"),
                    leader_price: d("0.4"),
                    copied_value: d("4"),
                    simulated_copy_price: d("0.4"),
                    quantity: d("10"),
                    copy_side: "buy".into(),
                    outcome: "Yes".into(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
                MovementRecord {
                    movement_id: "s1".into(),
                    market: "eth-updown-5m-1772281500".into(),
                    timestamp: "2026-02-28T12:01:00Z".into(),
                    leader_value: d("4"),
                    leader_price: d("0.5"),
                    copied_value: d("2"),
                    simulated_copy_price: d("0.5"),
                    quantity: d("4"),
                    copy_side: "sell".into(),
                    outcome: "Yes".into(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
            ],
        };

        // Remaining inventory: 10 - 4 = 6 shares.
        assert!(has_enough_inventory_for_sell(
            &state,
            "eth-updown-5m-1772281500",
            "Yes",
            d("5"),
        ));
        assert!(!has_enough_inventory_for_sell(
            &state,
            "eth-updown-5m-1772281500",
            "Yes",
            d("7"),
        ));
        assert!(!has_enough_inventory_for_sell(
            &state,
            "eth-updown-5m-1772281500",
            "No",
            d("1"),
        ));
    }

    #[test]
    fn fast_market_fee_detection_and_impact() {
        assert!(is_fast_market_with_fee("eth-updown-5m-1772281500"));
        assert!(is_fast_market_with_fee("btc-updown-15m-1772281500"));
        assert!(!is_fast_market_with_fee("btc-updown-1h-1772281500"));

        let impact = trading_fee_impact_for_movement("eth-updown-5m-1772281500", d("10")).unwrap();
        assert_eq!(impact.fee_bps, FAST_MARKET_FEE_BPS);
        assert_eq!(impact.entry_fee_usd, d("0.07"));
        assert_eq!(impact.round_trip_fee_usd, d("0.14"));
        assert_eq!(impact.max_gross_profit_usd, d("9"));
        assert_eq!(impact.max_net_profit_usd, d("8.86"));
    }

    #[test]
    fn normalize_market_slug_strips_numeric_suffix() {
        assert_eq!(
            normalize_market_slug("xrp-updown-5m-1772278200"),
            "xrp-updown-5m"
        );
        assert_eq!(normalize_market_slug("btc-updown-1h"), "btc-updown-1h");
    }

    #[test]
    fn resolved_market_settlement_marks_losing_buy_as_full_loss() {
        let mut state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "m-loss".into(),
                market: "highest-temperature-in-lucknow-on-march-8-2026-39c".into(),
                timestamp: "2026-03-08T10:00:00Z".into(),
                leader_value: d("39"),
                leader_price: d("0.999"),
                copied_value: d("39"),
                simulated_copy_price: d("0.999"),
                quantity: d("39"),
                copy_side: "buy".into(),
                outcome: "No".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };

        let resolved_outcomes = HashMap::from([(
            "highest-temperature-in-lucknow-on-march-8-2026-39c".to_string(),
            "Yes".to_string(),
        )]);

        let settled = settle_open_buys_from_resolved_markets(&mut state, &resolved_outcomes);
        assert_eq!(settled.len(), 1);
        assert!(state.movements[0].settled);
        assert_eq!(state.movements[0].resolved_outcome, "Yes");
        assert_eq!(state.movements[0].copy_side, "sell");
        assert_eq!(state.movements[0].pnl, d("-39"));
    }

    #[test]
    fn merge_or_redeem_activity_settles_matching_open_buy() {
        use polymarket_client_sdk::data::types::response::Activity;

        let mut state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "m-1".into(),
                market: "highest-temperature-in-lucknow-on-march-5-2026-40c".into(),
                timestamp: "2026-03-05T10:00:00Z".into(),
                leader_value: d("100"),
                leader_price: d("0.6"),
                copied_value: d("50"),
                simulated_copy_price: d("0.6"),
                quantity: d("80"),
                copy_side: "buy".into(),
                outcome: "No".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };

        let activities: Vec<Activity> = serde_json::from_value(serde_json::json!([
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "timestamp": 1772802272,
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "type": "MERGE",
                "size": "80",
                "usdcSize": "48",
                "transactionHash": "0x0000000000000000000000000000000000000000000000000000000000000011",
                "price": "0.6",
                "asset": "1",
                "side": "SELL",
                "outcomeIndex": 1,
                "title": "t",
                "slug": "highest-temperature-in-lucknow-on-march-5-2026-40c",
                "icon": "",
                "eventSlug": "e",
                "outcome": "No",
                "name": "",
                "pseudonym": "",
                "bio": "",
                "profileImage": "",
                "profileImageOptimized": ""
            }
        ])).unwrap();

        let settled = settle_open_buys_from_activities(&mut state, &activities);
        assert_eq!(settled.len(), 1);
        assert!(state.movements[0].settled);
        assert_eq!(state.movements[0].copy_side, "sell");
        assert_eq!(state.movements[0].resolved_outcome, "No");
    }

    #[test]
    fn sell_trade_without_open_buy_does_not_settle_anything() {
        let mut state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "only-buy-other-outcome".into(),
                market: "highest-temperature-in-lucknow-on-march-5-2026-40c".into(),
                timestamp: "2026-03-06T13:00:00Z".into(),
                leader_value: d("10"),
                leader_price: d("0.6"),
                copied_value: d("5"),
                simulated_copy_price: d("0.6"),
                quantity: d("8"),
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };

        let settled = settle_open_buys_from_sell_trade(
            &mut state,
            "highest-temperature-in-lucknow-on-march-5-2026-40c",
            "No",
            d("0.001"),
        );

        assert!(settled.is_empty());
        assert!(!state.movements[0].settled);
        assert_eq!(state.movements[0].copy_side, "buy");
    }

    #[test]
    fn sell_trade_settles_open_buy_with_loss() {
        let mut state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "b1".into(),
                market: "highest-temperature-in-ankara-on-march-7-2026-3c".into(),
                timestamp: "2026-03-06T09:00:00Z".into(),
                leader_value: d("473.90945"),
                leader_price: d("0.9873113541666668"),
                copied_value: d("98.49152826961924982793805508"),
                simulated_copy_price: d("0.999"),
                quantity: d("480"),
                copy_side: "buy".into(),
                outcome: "No".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };

        let settled = settle_open_buys_from_sell_trade(
            &mut state,
            "highest-temperature-in-ankara-on-march-7-2026-3c",
            "No",
            d("0.984"),
        );

        assert_eq!(settled.len(), 1);
        assert!(state.movements[0].settled);
        assert_eq!(state.movements[0].copy_side, "sell");
        assert!(state.movements[0].pnl < Decimal::ZERO);
    }

    #[test]
    fn unsettled_market_slugs_normalizes_and_deduplicates() {
        let state = CopyState {
            movements: vec![
                MovementRecord {
                    movement_id: "1".into(),
                    market: "btc-updown-5m-1772278200".into(),
                    timestamp: "2025-01-01T00:00:00Z".into(),
                    leader_value: d("10"),
                    leader_price: Decimal::ZERO,
                    copied_value: d("1"),
                    simulated_copy_price: Decimal::ZERO,
                    quantity: Decimal::ZERO,
                    copy_side: "buy".into(),
                    outcome: "No".into(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
                MovementRecord {
                    movement_id: "2".into(),
                    market: "btc-updown-5m-1772278300".into(),
                    timestamp: "2025-01-01T00:01:00Z".into(),
                    leader_value: d("10"),
                    leader_price: Decimal::ZERO,
                    copied_value: d("1"),
                    simulated_copy_price: Decimal::ZERO,
                    quantity: Decimal::ZERO,
                    copy_side: "buy".into(),
                    outcome: String::new(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
                MovementRecord {
                    movement_id: "3".into(),
                    market: "eth-updown-5m-1772278300".into(),
                    timestamp: "2025-01-01T00:02:00Z".into(),
                    leader_value: d("10"),
                    leader_price: Decimal::ZERO,
                    copied_value: d("1"),
                    simulated_copy_price: Decimal::ZERO,
                    quantity: Decimal::ZERO,
                    copy_side: "buy".into(),
                    outcome: String::new(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: true,
                    pnl: Decimal::ZERO,
                },
            ],
        };

        let slugs = unsettled_market_slugs(&state);
        assert_eq!(slugs, vec!["btc-updown-5m".to_string()]);
    }

    #[test]
    fn oldest_unsettled_db_row_selects_lowest_id_not_settled() {
        let rows = vec![
            DbRow {
                id: 2,
                movement_id: "b".into(),
                market: "m2".into(),
                timestamp: "2025-01-01T00:00:01Z".into(),
                leader_value: "10".into(),
                leader_price: "0".into(),
                copied_value: "5".into(),
                simulated_copy_price: "0".into(),
                quantity: "0".into(),
                copy_side: "unknown".into(),
                outcome: String::new(),
                resolved_outcome: String::new(),
                diff_pct: "0".into(),
                estimated_total_fee_usd: "0".into(),
                settled: false,
                pnl: "0".into(),
            },
            DbRow {
                id: 1,
                movement_id: "a".into(),
                market: "m1".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
                leader_value: "10".into(),
                leader_price: "0".into(),
                copied_value: "5".into(),
                simulated_copy_price: "0".into(),
                quantity: "0".into(),
                copy_side: "unknown".into(),
                outcome: String::new(),
                resolved_outcome: String::new(),
                diff_pct: "0".into(),
                estimated_total_fee_usd: "0".into(),
                settled: true,
                pnl: "1".into(),
            },
            DbRow {
                id: 3,
                movement_id: "c".into(),
                market: "m3".into(),
                timestamp: "2025-01-01T00:00:02Z".into(),
                leader_value: "10".into(),
                leader_price: "0".into(),
                copied_value: "5".into(),
                simulated_copy_price: "0".into(),
                quantity: "0".into(),
                copy_side: "unknown".into(),
                outcome: String::new(),
                resolved_outcome: String::new(),
                diff_pct: "0".into(),
                estimated_total_fee_usd: "0".into(),
                settled: false,
                pnl: "0".into(),
            },
        ];

        let oldest = oldest_unsettled_db_row(&rows).expect("expected oldest unsettled row");
        assert_eq!(oldest.id, 2);
        assert_eq!(oldest.movement_id, "b");
    }

    #[test]
    fn settlement_pnl_is_scaled_by_invested_amount() {
        let pnl = calculate_settlement_pnl_from_invested(d("25"), d("100"), d("15"));
        assert_eq!(pnl, d("3.75"));
    }

    #[test]
    fn settle_open_movements_uses_position_roi_sequence_and_keeps_negative_pnl() {
        use polymarket_client_sdk::data::types::response::ClosedPosition;

        let mut state = CopyState {
            movements: vec![
                MovementRecord {
                    movement_id: "m1".into(),
                    market: "btc-updown-5m-1772278200".into(),
                    timestamp: "2025-01-01T00:00:00Z".into(),
                    leader_value: d("100"),
                    leader_price: Decimal::ZERO,
                    copied_value: d("10"),
                    simulated_copy_price: Decimal::ZERO,
                    quantity: Decimal::ZERO,
                    copy_side: "unknown".into(),
                    outcome: "Yes".into(),
                    resolved_outcome: String::new().into(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
                MovementRecord {
                    movement_id: "m2".into(),
                    market: "btc-updown-5m-1772278300".into(),
                    timestamp: "2025-01-01T00:05:00Z".into(),
                    leader_value: d("100"),
                    leader_price: Decimal::ZERO,
                    copied_value: d("8"),
                    simulated_copy_price: Decimal::ZERO,
                    quantity: Decimal::ZERO,
                    copy_side: "unknown".into(),
                    outcome: "No".into(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
            ],
        };

        let closed: Vec<ClosedPosition> = serde_json::from_value(serde_json::json!([
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "asset": "1",
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "avgPrice": "0.5",
                "totalBought": "20",
                "realizedPnl": "-4",
                "curPrice": "0",
                "timestamp": 1735689600,
                "title": "t",
                "slug": "btc-updown-5m",
                "icon": "",
                "eventSlug": "e",
                "outcome": "Yes",
                "outcomeIndex": 0,
                "oppositeOutcome": "No",
                "oppositeAsset": "2",
                "endDate": "2025-01-01T00:00:00Z"
            },
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "asset": "3",
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "avgPrice": "0.5",
                "totalBought": "10",
                "realizedPnl": "2",
                "curPrice": "0",
                "timestamp": 1735689900,
                "title": "t",
                "slug": "btc-updown-5m",
                "icon": "",
                "eventSlug": "e",
                "outcome": "No",
                "outcomeIndex": 1,
                "oppositeOutcome": "Yes",
                "oppositeAsset": "4",
                "endDate": "2025-01-01T00:00:00Z"
            }
        ]))
        .unwrap();

        let settled = settle_open_movements_from_closed_positions(&mut state, &closed);
        assert_eq!(settled.len(), 2);
        assert_eq!(state.movements[0].pnl, d("-2"));
        assert_eq!(state.movements[1].pnl, d("1.6"));
    }

    #[test]
    fn settle_matches_closed_positions_by_slug_and_outcome() {
        use polymarket_client_sdk::data::types::response::ClosedPosition;

        let mut state = CopyState {
            movements: vec![
                MovementRecord {
                    movement_id: "yes-mov".into(),
                    market: "btc-updown-5m-1772278200".into(),
                    timestamp: "2025-01-01T00:00:00Z".into(),
                    leader_value: d("100"),
                    leader_price: Decimal::ZERO,
                    copied_value: d("10"),
                    simulated_copy_price: Decimal::ZERO,
                    quantity: Decimal::ZERO,
                    copy_side: "buy".into(),
                    outcome: "Yes".into(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
                MovementRecord {
                    movement_id: "no-mov".into(),
                    market: "btc-updown-5m-1772278300".into(),
                    timestamp: "2025-01-01T00:01:00Z".into(),
                    leader_value: d("100"),
                    leader_price: Decimal::ZERO,
                    copied_value: d("10"),
                    simulated_copy_price: Decimal::ZERO,
                    quantity: Decimal::ZERO,
                    copy_side: "buy".into(),
                    outcome: "No".into(),
                    resolved_outcome: String::new(),
                    diff_pct: Decimal::ZERO,
                    estimated_total_fee_usd: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                },
            ],
        };

        // Closed positions come in opposite outcome order vs movements.
        let closed: Vec<ClosedPosition> = serde_json::from_value(serde_json::json!([
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "asset": "1",
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "avgPrice": "0.5",
                "totalBought": "10",
                "realizedPnl": "5",
                "curPrice": "0",
                "timestamp": 1735689660,
                "title": "t",
                "slug": "btc-updown-5m",
                "icon": "",
                "eventSlug": "e",
                "outcome": "No",
                "outcomeIndex": 1,
                "oppositeOutcome": "Yes",
                "oppositeAsset": "2",
                "endDate": "2025-01-01T00:00:00Z"
            },
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "asset": "3",
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "avgPrice": "0.5",
                "totalBought": "10",
                "realizedPnl": "-2",
                "curPrice": "0",
                "timestamp": 1735689720,
                "title": "t",
                "slug": "btc-updown-5m",
                "icon": "",
                "eventSlug": "e",
                "outcome": "Yes",
                "outcomeIndex": 0,
                "oppositeOutcome": "No",
                "oppositeAsset": "4",
                "endDate": "2025-01-01T00:00:00Z"
            }
        ]))
        .unwrap();

        let settled = settle_open_movements_from_closed_positions(&mut state, &closed);
        assert_eq!(settled.len(), 2);

        // Must match by outcome, not just slug/order.
        assert_eq!(state.movements[0].resolved_outcome, "Yes");
        assert_eq!(state.movements[0].copy_side, "sell");
        assert_eq!(state.movements[0].pnl, d("-2"));
        assert_eq!(state.movements[1].resolved_outcome, "No");
        assert_eq!(state.movements[1].copy_side, "sell");
        assert_eq!(state.movements[1].pnl, d("5"));
    }

    #[test]
    fn settle_allows_unknown_closed_timestamp_zero() {
        use polymarket_client_sdk::data::types::response::ClosedPosition;

        let mut state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "m-zero-ts".into(),
                market: "eth-updown-5m-1772281500".into(),
                timestamp: "2026-02-28T12:30:00Z".into(),
                leader_value: d("20"),
                leader_price: Decimal::ZERO,
                copied_value: d("10"),
                simulated_copy_price: Decimal::ZERO,
                quantity: Decimal::ZERO,
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };

        let closed: Vec<ClosedPosition> = serde_json::from_value(serde_json::json!([
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "asset": "1",
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "avgPrice": "0.5",
                "totalBought": "20",
                "realizedPnl": "2",
                "curPrice": "0",
                "timestamp": 0,
                "title": "t",
                "slug": "eth-updown-5m",
                "icon": "",
                "eventSlug": "e",
                "outcome": "Yes",
                "outcomeIndex": 0,
                "oppositeOutcome": "No",
                "oppositeAsset": "2",
                "endDate": "2025-01-01T00:00:00Z"
            }
        ]))
        .unwrap();

        let settled = settle_open_movements_from_closed_positions(&mut state, &closed);
        assert_eq!(settled.len(), 1);
        assert!(state.movements[0].settled);
        assert_eq!(state.movements[0].pnl, d("1"));
    }

    #[test]
    fn settle_fallback_uses_old_closure_when_no_eligible_timestamp_exists() {
        use polymarket_client_sdk::data::types::response::ClosedPosition;

        let mut state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "m-fallback".into(),
                market: "eth-updown-5m-1772281500".into(),
                timestamp: "2026-02-28T12:30:00Z".into(),
                leader_value: d("20"),
                leader_price: Decimal::ZERO,
                copied_value: d("10"),
                simulated_copy_price: Decimal::ZERO,
                quantity: Decimal::ZERO,
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };

        let closed: Vec<ClosedPosition> = serde_json::from_value(serde_json::json!([
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "asset": "1",
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "avgPrice": "0.5",
                "totalBought": "20",
                "realizedPnl": "2",
                "curPrice": "0",
                "timestamp": 1735689600,
                "title": "t",
                "slug": "eth-updown-5m",
                "icon": "",
                "eventSlug": "e",
                "outcome": "Yes",
                "outcomeIndex": 0,
                "oppositeOutcome": "No",
                "oppositeAsset": "2",
                "endDate": "2025-01-01T00:00:00Z"
            }
        ]))
        .unwrap();

        let settled = settle_open_movements_from_closed_positions(&mut state, &closed);
        assert_eq!(settled.len(), 1);
        assert!(state.movements[0].settled);
        assert_eq!(state.movements[0].pnl, d("1"));
    }

    #[test]
    fn settle_does_not_close_when_market_does_not_match() {
        use polymarket_client_sdk::data::types::response::ClosedPosition;

        let mut state = CopyState {
            movements: vec![MovementRecord {
                movement_id: "m-new".into(),
                market: "eth-updown-5m-1772281500".into(),
                timestamp: "2026-02-28T12:30:00Z".into(),
                leader_value: d("20"),
                leader_price: Decimal::ZERO,
                copied_value: d("10"),
                simulated_copy_price: Decimal::ZERO,
                quantity: Decimal::ZERO,
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: false,
                pnl: Decimal::ZERO,
            }],
        };

        let closed: Vec<ClosedPosition> = serde_json::from_value(serde_json::json!([
            {
                "proxyWallet": "0x0000000000000000000000000000000000000001",
                "asset": "1",
                "conditionId": "0x0000000000000000000000000000000000000000000000000000000000000000",
                "avgPrice": "0.5",
                "totalBought": "20",
                "realizedPnl": "2",
                "curPrice": "0",
                "timestamp": 1735689600,
                "title": "t",
                "slug": "btc-updown-5m",
                "icon": "",
                "eventSlug": "e",
                "outcome": "Yes",
                "outcomeIndex": 0,
                "oppositeOutcome": "No",
                "oppositeAsset": "2",
                "endDate": "2025-01-01T00:00:00Z"
            }
        ]))
        .unwrap();

        let settled = settle_open_movements_from_closed_positions(&mut state, &closed);
        assert!(settled.is_empty());
        assert!(!state.movements[0].settled);
    }
    #[test]
    fn daily_series_groups_by_hour() {
        let movements = vec![
            MovementRecord {
                movement_id: "m1".into(),
                market: "mkt".into(),
                timestamp: "2026-02-28T12:01:00Z".into(),
                leader_value: d("10"),
                leader_price: d("0.5"),
                copied_value: d("5"),
                simulated_copy_price: d("0.52"),
                quantity: d("10"),
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: true,
                pnl: d("1.5"),
            },
            MovementRecord {
                movement_id: "m2".into(),
                market: "mkt".into(),
                timestamp: "2026-02-28T12:40:00Z".into(),
                leader_value: d("10"),
                leader_price: d("0.5"),
                copied_value: d("5"),
                simulated_copy_price: d("0.52"),
                quantity: d("10"),
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: true,
                pnl: d("0.5"),
            },
            MovementRecord {
                movement_id: "m3".into(),
                market: "mkt".into(),
                timestamp: "2026-02-28T13:10:00Z".into(),
                leader_value: d("10"),
                leader_price: d("0.5"),
                copied_value: d("5"),
                simulated_copy_price: d("0.52"),
                quantity: d("10"),
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: String::new(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: Decimal::ZERO,
                settled: true,
                pnl: d("2"),
            },
        ];

        let series = daily_pnl_series(&movements);
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].0, "2026-02-28 12:00");
        assert_eq!(series[0].1, d("2.0"));
        assert_eq!(series[1].0, "2026-02-28 13:00");
        assert_eq!(series[1].1, d("2"));
    }
    #[test]
    fn daily_series_uses_net_pnl_after_fees() {
        let movements = vec![MovementRecord {
            movement_id: "m-net".into(),
            market: "mkt".into(),
            timestamp: "2026-02-28T12:01:00Z".into(),
            leader_value: d("10"),
            leader_price: d("0.5"),
            copied_value: d("5"),
            simulated_copy_price: d("0.52"),
            quantity: d("10"),
            copy_side: "buy".into(),
            outcome: "Yes".into(),
            resolved_outcome: String::new(),
            diff_pct: Decimal::ZERO,
            estimated_total_fee_usd: d("0.2"),
            settled: true,
            pnl: d("1.0"),
        }];

        let series = daily_pnl_series(&movements);
        assert_eq!(series.len(), 1);
        assert_eq!(series[0].0, "2026-02-28 12:00");
        assert_eq!(series[0].1, d("0.8"));
    }
    #[test]
    fn plan_uses_current_equity_after_settled_pnl_and_fees() {
        let cfg = CopyConfig {
            leader: "0x1".into(),
            allocated_funds: d("1000"),
            max_trade_pct: d("10"),
            max_total_exposure_pct: d("50"),
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
                movement_id: "s1".into(),
                market: "mkt".into(),
                timestamp: "2026-03-01T10:00:00Z".into(),
                leader_value: d("100"),
                leader_price: d("0.5"),
                copied_value: d("50"),
                simulated_copy_price: d("0.5"),
                quantity: d("100"),
                copy_side: "buy".into(),
                outcome: "Yes".into(),
                resolved_outcome: "Yes".into(),
                diff_pct: Decimal::ZERO,
                estimated_total_fee_usd: d("10"),
                settled: true,
                pnl: d("210"),
            }],
        };

        let plan = compute_plan(&cfg, &state, d("1000"), d("200")).unwrap();
        // Equity = 1000 + (210 - 10) = 1200; proportional = 200 * 1.2 = 240
        // max_trade = 120 and max_total_exposure = 600, so capped = 120.
        assert_eq!(plan.proportional_size, d("240"));
        assert_eq!(plan.capped_size, d("120"));
        assert_eq!(plan.available_funds, d("600"));
    }
}
