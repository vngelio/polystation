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
    /// Configure copy-trading profile and risk parameters
    Configure(ConfigureArgs),
    /// Show current copy-trader status and funds usage
    Status,
    /// Compute proportional copy size for a detected leader movement
    Plan(PlanArgs),
    /// Record a copied movement for dashboard tracking
    Record(RecordArgs),
    /// Settle a resolved bet and release funds/PnL back into available capital
    Settle(SettleArgs),
    /// Print dashboard with copied movements and PnL charts
    Dashboard,
    /// Launch a local web UI for monitoring and controlling copy mode
    Ui(UiArgs),
}

#[derive(Args)]
pub struct UiArgs {
    /// Host to bind the local UI server
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Port for the local UI server
    #[arg(long, default_value_t = 8787)]
    pub port: u16,
}

#[derive(Args, Serialize, Deserialize)]
pub struct ConfigureArgs {
    /// Leader account address to follow
    #[arg(long)]
    pub leader: String,
    /// Capital assigned to copy-trading in USD units
    #[arg(long)]
    pub allocated_funds: Decimal,
    /// Max percentage of allocated funds per copied trade (0-100)
    #[arg(long, default_value_t = Decimal::from_i128_with_scale(500, 2))]
    pub max_trade_pct: Decimal,
    /// Max total exposure percentage of allocated funds (0-100)
    #[arg(long, default_value_t = Decimal::from_i128_with_scale(7000, 2))]
    pub max_total_exposure_pct: Decimal,
    /// Skip copy operations smaller than this amount
    #[arg(long, default_value_t = Decimal::ONE)]
    pub min_copy_usd: Decimal,
    /// Monitor poll interval in seconds
    #[arg(long, default_value_t = 10)]
    pub poll_interval_secs: u64,
    /// Risk profile preset for future strategy tuning
    #[arg(long, value_enum, default_value_t = RiskLevel::Balanced)]
    pub risk_level: RiskLevel,
    /// If true, auto-copy mode will try to execute orders (experimental)
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
    /// Current leader total position value in USD
    #[arg(long)]
    pub leader_positions_value: Decimal,
    /// Detected leader movement amount in USD
    #[arg(long)]
    pub leader_movement_value: Decimal,
}

#[derive(Args)]
pub struct RecordArgs {
    /// ID of market/order reference to track
    #[arg(long)]
    pub movement_id: String,
    /// Market slug/condition label
    #[arg(long)]
    pub market: String,
    /// Leader amount in USD
    #[arg(long)]
    pub leader_value: Decimal,
    /// Copied amount in USD
    #[arg(long)]
    pub copied_value: Decimal,
    /// Execution difference vs leader (% points, can be negative)
    #[arg(long, default_value_t = Decimal::ZERO)]
    pub diff_pct: Decimal,
}

#[derive(Args)]
pub struct SettleArgs {
    /// Movement ID to settle
    #[arg(long)]
    pub movement_id: String,
    /// Realized PnL in USD for that movement
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
            save_config(&CopyConfig {
                leader: cfg.leader,
                allocated_funds: cfg.allocated_funds,
                max_trade_pct: cfg.max_trade_pct,
                max_total_exposure_pct: cfg.max_total_exposure_pct,
                min_copy_usd: cfg.min_copy_usd,
                poll_interval_secs: cfg.poll_interval_secs,
                risk_level: cfg.risk_level,
                execute_orders: cfg.execute_orders,
            })?;
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
            state.movements.push(entry);
            save_state(&state)?;
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
    state: CopyState,
    monitoring: bool,
    last_seen_hashes: HashSet<String>,
}

#[derive(Serialize)]
struct UiStateResponse {
    configured: bool,
    monitoring: bool,
    config: Option<CopyConfig>,
    movements: Vec<MovementRecord>,
    daily_pnl: Vec<(String, Decimal)>,
    historical_pnl: Vec<(String, Decimal)>,
}

async fn run_ui(ui: UiArgs) -> Result<()> {
    let app_state = UiAppState {
        runtime: Arc::new(Mutex::new(RuntimeState {
            config: load_config().ok(),
            state: load_state().unwrap_or_default(),
            monitoring: false,
            last_seen_hashes: HashSet::new(),
        })),
    };

    let addr = format!("{}:{}", ui.host, ui.port);
    println!("Copy UI running at http://{addr}");
    let listener = TcpListener::bind(&addr)?;

    loop {
        let (stream, _) = listener.accept()?;
        let app = app_state.clone();
        tokio::spawn(async move {
            let _ = handle_http(stream, app).await;
        });
    }
}

async fn handle_http(mut stream: TcpStream, app: UiAppState) -> Result<()> {
    let mut buf = vec![0_u8; 1024 * 64];
    let read = stream.read(&mut buf)?;
    let req = String::from_utf8_lossy(&buf[..read]);
    let mut lines = req.lines();
    let first = lines.next().unwrap_or_default();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let body = req
        .split(
            "

",
        )
        .nth(1)
        .unwrap_or("");

    match (method, path) {
        ("GET", "/") => write_response(
            &mut stream,
            "200 OK",
            "text/html",
            include_str!("../output/copy_ui.html"),
        )?,
        ("GET", "/api/state") => {
            let payload = {
                let runtime = app.runtime.lock().await;
                serde_json::to_string(&UiStateResponse {
                    configured: runtime.config.is_some(),
                    monitoring: runtime.monitoring,
                    config: runtime.config.clone(),
                    movements: runtime.state.movements.clone(),
                    daily_pnl: daily_pnl_series(&runtime.state.movements),
                    historical_pnl: cumulative_pnl_series(&runtime.state.movements),
                })?
            };
            write_response(&mut stream, "200 OK", "application/json", &payload)?;
        }
        ("POST", "/api/configure") => {
            let cfg: ConfigureArgs =
                serde_json::from_str(body).map_err(|_| anyhow!("invalid json"))?;
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

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}
Content-Type: {content_type}
Content-Length: {}
Connection: close

{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes())?;
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

        let trades_req = TradesRequest::builder().user(leader).limit(25)?.build();
        let trades = data_client.trades(&trades_req).await.unwrap_or_default();

        {
            let mut runtime = app.runtime.lock().await;
            for t in trades {
                let tx_hash = t.transaction_hash.to_string();
                if runtime.last_seen_hashes.contains(&tx_hash) {
                    continue;
                }
                runtime.last_seen_hashes.insert(tx_hash.clone());

                let leader_move = t.size * t.price;
                let plan = compute_plan(&cfg, &runtime.state, leader_value, leader_move)?;
                if plan.capped_size <= Decimal::ZERO {
                    continue;
                }

                runtime.state.movements.push(MovementRecord {
                    movement_id: tx_hash,
                    market: t.slug,
                    timestamp: Utc::now().to_rfc3339(),
                    leader_value: leader_move,
                    copied_value: plan.capped_size,
                    diff_pct: Decimal::ZERO,
                    settled: false,
                    pnl: Decimal::ZERO,
                });
            }
            let _ = save_state(&runtime.state);
        }

        tokio::time::sleep(Duration::from_secs(cfg.poll_interval_secs.max(1))).await;
    }
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

fn save_config(cfg: &CopyConfig) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("Failed creating config directory")?;
    }
    fs::write(path, serde_json::to_string_pretty(cfg)?)
        .context("Failed writing copy-trader config")?;
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
        fs::create_dir_all(parent).context("Failed creating config directory")?;
    }
    fs::write(path, serde_json::to_string_pretty(state)?).context("Failed writing state")?;
    Ok(())
}

fn load_state() -> Result<CopyState> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(CopyState::default());
    }
    let data = fs::read_to_string(path).context("Failed reading state")?;
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
            poll_interval_secs: 10,
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
            poll_interval_secs: 10,
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

    #[test]
    fn daily_and_cumulative_pnl_series_work() {
        let movements = vec![
            MovementRecord {
                movement_id: "a".into(),
                market: "m".into(),
                timestamp: "2025-01-01T00:00:00Z".into(),
                leader_value: d("10"),
                copied_value: d("10"),
                diff_pct: Decimal::ZERO,
                settled: true,
                pnl: d("2"),
            },
            MovementRecord {
                movement_id: "b".into(),
                market: "m".into(),
                timestamp: "2025-01-01T12:00:00Z".into(),
                leader_value: d("10"),
                copied_value: d("10"),
                diff_pct: Decimal::ZERO,
                settled: true,
                pnl: d("3"),
            },
            MovementRecord {
                movement_id: "c".into(),
                market: "m".into(),
                timestamp: "2025-01-02T00:00:00Z".into(),
                leader_value: d("10"),
                copied_value: d("10"),
                diff_pct: Decimal::ZERO,
                settled: true,
                pnl: d("-1"),
            },
        ];
        let daily = daily_pnl_series(&movements);
        assert_eq!(daily[0].1, d("5"));
        assert_eq!(daily[1].1, d("-1"));
        let cumulative = cumulative_pnl_series(&movements);
        assert_eq!(cumulative[0].1, d("5"));
        assert_eq!(cumulative[1].1, d("4"));
    }
}
