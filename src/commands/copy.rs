use std::{
    collections::{BTreeMap, HashSet},
    fs,
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
use polymarket_client_sdk::data::types::request::{TradesRequest, ValueRequest};

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
    #[arg(long, value_enum, default_value_t = RiskLevel::Balanced)]
    pub risk_level: RiskLevel,
    #[arg(long, default_value_t = false)]
    pub execute_orders: bool,
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
    pub risk_level: RiskLevel,
    pub execute_orders: bool,
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
                risk_level: cfg.risk_level,
                execute_orders: cfg.execute_orders,
            };
            save_config(&c)?;
            init_db()?;
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
            append_db_movement(&entry)?;
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
            settle_db_movement(&settle.movement_id, settle.pnl)?;
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
    last_seen_hashes: HashSet<String>,
}

#[derive(Serialize)]
struct UiStateResponse {
    configured: bool,
    monitoring: bool,
    config: Option<CopyConfig>,
    movement_count: usize,
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

    init_db()?;
    let token = generate_api_token()?;
    let addr = format!("{}:{}", ui.host, ui.port);
    println!("Copy UI running at http://{addr}");
    println!("UI API token: {token}");

    let app_state = UiAppState {
        runtime: Arc::new(Mutex::new(RuntimeState {
            config: load_config().ok(),
            monitoring: false,
            last_seen_hashes: HashSet::new(),
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
            let db_state = load_state_from_db()?;
            let runtime = app.runtime.lock().await;
            let payload = serde_json::to_string(&UiStateResponse {
                configured: runtime.config.is_some(),
                monitoring: runtime.monitoring,
                config: runtime.config.clone(),
                movement_count: db_state.movements.len(),
                daily_pnl: daily_pnl_series(&db_state.movements),
                historical_pnl: cumulative_pnl_series(&db_state.movements),
            })?;
            write_response(&mut stream, "200 OK", "application/json", &payload)?;
        }
        ("GET", "/api/updates") => {
            let since = parse_since(query);
            let (latest_id, rows) = db_updates_since(since)?;
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
                risk_level: cfg.risk_level,
                execute_orders: cfg.execute_orders,
            };
            save_config(&config)?;
            let mut runtime = app.runtime.lock().await;
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
            write_response(&mut stream, "200 OK", "application/json", "{\"ok\":true}")?;
        }
        _ => write_response(&mut stream, "404 Not Found", "text/plain", "not found")?,
    }

    Ok(())
}

async fn monitor_loop(app: UiAppState) -> Result<()> {
    let data_client = polymarket_client_sdk::data::Client::default();
    loop {
        let (running, cfg) = {
            let runtime = app.runtime.lock().await;
            (runtime.monitoring, runtime.config.clone())
        };
        if !running {
            break;
        }
        let Some(cfg) = cfg else {
            break;
        };

        let leader = crate::commands::parse_address(&cfg.leader)?;
        let value_req = ValueRequest::builder().user(leader).build();
        let leader_value = data_client
            .value(&value_req)
            .await
            .ok()
            .and_then(|v| v.first().map(|x| x.value))
            .unwrap_or(Decimal::ONE);

        let trades_req = TradesRequest::builder().user(leader).limit(20)?.build();
        let trades = data_client.trades(&trades_req).await.unwrap_or_default();

        for t in trades {
            let tx_hash = t.transaction_hash.to_string();
            let mut runtime = app.runtime.lock().await;
            if runtime.last_seen_hashes.contains(&tx_hash) {
                continue;
            }
            runtime.last_seen_hashes.insert(tx_hash.clone());

            let state = load_state()?;
            let plan = compute_plan(&cfg, &state, leader_value, t.size * t.price)?;
            if plan.capped_size <= Decimal::ZERO {
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
            append_db_movement(&record)?;
        }

        tokio::time::sleep(Duration::from_millis(
            (cfg.poll_interval_secs.max(1)) * 1000,
        ))
        .await;
    }
    Ok(())
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
    let mut f = fs::File::open("/dev/urandom").context("failed to open /dev/urandom")?;
    let mut buf = [0u8; 32];
    f.read_exact(&mut buf)?;
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

fn db_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("copy_trader_db.jsonl"))
}

fn init_db() -> Result<()> {
    let path = db_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(path, "")?;
    }
    Ok(())
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

fn read_db_rows() -> Result<Vec<DbRow>> {
    init_db()?;
    let raw = fs::read_to_string(db_path()?)?;
    let mut out = Vec::new();
    for line in raw.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(v) = serde_json::from_str::<DbRow>(line) {
            out.push(v);
        }
    }
    out.sort_by_key(|x| x.id);
    Ok(out)
}

fn write_db_rows(rows: &[DbRow]) -> Result<()> {
    let mut body = String::new();
    for r in rows {
        body.push_str(&serde_json::to_string(r)?);
        body.push('\n');
    }
    fs::write(db_path()?, body)?;
    Ok(())
}

fn append_db_movement(m: &MovementRecord) -> Result<()> {
    let mut rows = read_db_rows()?;
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
    write_db_rows(&rows)
}

fn settle_db_movement(movement_id: &str, pnl: Decimal) -> Result<()> {
    let mut rows = read_db_rows()?;
    for r in &mut rows {
        if r.movement_id == movement_id {
            r.settled = true;
            r.pnl = pnl.to_string();
        }
    }
    write_db_rows(&rows)
}

fn load_state_from_db() -> Result<CopyState> {
    let rows = read_db_rows()?;
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

fn db_updates_since(since: i64) -> Result<(i64, Vec<DbMovement>)> {
    let rows = read_db_rows()?;
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
            risk_level: RiskLevel::Balanced,
            execute_orders: false,
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
            risk_level: RiskLevel::Balanced,
            execute_orders: false,
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
}
