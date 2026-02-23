use anyhow::Result;
use clap::{Args, Subcommand};
use polymarket_client_sdk::bridge::{
    self,
    types::{DepositRequest, StatusRequest},
};
use polymarket_client_sdk::types::Address;

use crate::output::OutputFormat;
use crate::output::bridge::{print_deposit, print_status, print_supported_assets};

#[derive(Args)]
pub struct BridgeArgs {
    #[command(subcommand)]
    pub command: BridgeCommand,
}

#[derive(Subcommand)]
pub enum BridgeCommand {
    /// Get deposit addresses for a wallet (EVM, Solana, Bitcoin)
    Deposit {
        /// Polymarket wallet address (0x...)
        address: String,
    },

    /// List supported chains and tokens for deposits
    SupportedAssets,

    /// Check deposit transaction status for an address
    Status {
        /// Deposit address (EVM, Solana, or Bitcoin)
        address: String,
    },
}

fn parse_address(s: &str) -> Result<Address> {
    s.parse()
        .map_err(|_| anyhow::anyhow!("Invalid address: must be a 0x-prefixed hex address"))
}

pub async fn execute(
    client: &bridge::Client,
    args: BridgeArgs,
    output: OutputFormat,
) -> Result<()> {
    match args.command {
        BridgeCommand::Deposit { address } => {
            let request = DepositRequest::builder()
                .address(parse_address(&address)?)
                .build();

            let response = client.deposit(&request).await?;
            print_deposit(&response, &output);
        }

        BridgeCommand::SupportedAssets => {
            let response = client.supported_assets().await?;
            print_supported_assets(&response, &output);
        }

        BridgeCommand::Status { address } => {
            let request = StatusRequest::builder()
                .address(&address)
                .build();

            let response = client.status(&request).await?;
            print_status(&response, &output);
        }
    }

    Ok(())
}
