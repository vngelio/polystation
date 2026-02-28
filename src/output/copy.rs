use anyhow::Result;
use rust_decimal::Decimal;
use serde::Serialize;

use crate::{
    commands::copy::{CopyState, PlanResult, cumulative_pnl_series, daily_pnl_series},
    output::OutputFormat,
};

#[derive(Serialize)]
struct StatusView<'a> {
    leader: &'a str,
    allocated_funds: Decimal,
    open_movements: usize,
    settled_movements: usize,
    open_exposure: Decimal,
    realized_pnl: Decimal,
}

pub fn print_status(
    config: &crate::commands::copy::CopyConfig,
    state: &CopyState,
    output: OutputFormat,
) -> Result<()> {
    let open_movements = state.movements.iter().filter(|m| !m.settled).count();
    let settled_movements = state.movements.iter().filter(|m| m.settled).count();
    let open_exposure: Decimal = state
        .movements
        .iter()
        .filter(|m| !m.settled)
        .map(|m| m.copied_value)
        .sum();
    let realized_pnl: Decimal = state
        .movements
        .iter()
        .filter(|m| m.settled)
        .map(|m| m.pnl)
        .sum();

    let view = StatusView {
        leader: &config.leader,
        allocated_funds: config.allocated_funds,
        open_movements,
        settled_movements,
        open_exposure,
        realized_pnl,
    };

    match output {
        OutputFormat::Json => crate::output::print_json(&view),
        OutputFormat::Table => {
            crate::output::print_detail_table(vec![
                ["Leader".into(), view.leader.to_string()],
                ["Allocated funds".into(), view.allocated_funds.to_string()],
                ["Open movements".into(), view.open_movements.to_string()],
                [
                    "Settled movements".into(),
                    view.settled_movements.to_string(),
                ],
                ["Open exposure".into(), view.open_exposure.to_string()],
                ["Realized PnL".into(), view.realized_pnl.to_string()],
            ]);
            Ok(())
        }
    }
}

pub fn print_plan(result: &PlanResult, output: OutputFormat) -> Result<()> {
    match output {
        OutputFormat::Json => crate::output::print_json(result),
        OutputFormat::Table => {
            crate::output::print_detail_table(vec![
                [
                    "Proportional size".into(),
                    result.proportional_size.to_string(),
                ],
                ["Planned copy size".into(), result.capped_size.to_string()],
                ["Available funds".into(), result.available_funds.to_string()],
                ["Reason".into(), result.reason.clone()],
            ]);
            Ok(())
        }
    }
}

pub fn print_dashboard(state: &CopyState, output: OutputFormat) -> Result<()> {
    if matches!(output, OutputFormat::Json) {
        return crate::output::print_json(&serde_json::json!({
            "movements": state.movements,
            "daily_pnl": daily_pnl_series(&state.movements),
            "historical_pnl": cumulative_pnl_series(&state.movements),
        }));
    }

    println!("Copied movements:");
    if state.movements.is_empty() {
        println!("  (none)");
    } else {
        for m in &state.movements {
            println!(
                "- {} | {} | leader_px={} | sim_px={} | qty={} | copied={} | diff={}pp | settled={} | pnl={}",
                m.timestamp,
                m.market,
                m.leader_price,
                m.simulated_copy_price,
                m.quantity,
                m.copied_value,
                m.diff_pct,
                m.settled,
                m.pnl
            );
        }
    }

    println!("\nDaily PnL:");
    for (day, pnl) in daily_pnl_series(&state.movements) {
        println!("{} {} {pnl}", day, bar(pnl));
    }

    println!("\nHistorical PnL:");
    for (day, pnl) in cumulative_pnl_series(&state.movements) {
        println!("{} {} {pnl}", day, bar(pnl));
    }
    Ok(())
}

fn bar(v: Decimal) -> String {
    let abs = v.abs().to_i32().unwrap_or(0).clamp(0, 40) as usize;
    if v.is_sign_negative() {
        format!("{}|", "-".repeat(abs))
    } else {
        format!("|{}", "+".repeat(abs))
    }
}

trait ToI32 {
    fn to_i32(&self) -> Option<i32>;
}

impl ToI32 for Decimal {
    fn to_i32(&self) -> Option<i32> {
        self.trunc().to_string().parse().ok()
    }
}
