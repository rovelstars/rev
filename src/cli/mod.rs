use clap::Parser;
pub mod parse_service;
mod commands;
use crate::dashboard;

#[derive(Parser, Debug)]
#[command(name = "rev")]
#[command(about = "RunixOS Service Manager", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: commands::Commands,
}

pub async fn run(args: &[String]) {
    if args.len() == 1 {
        let _ = dashboard::show().await;
    } else {
        let cli = Cli::parse_from(args.iter().map(|s| s.as_str()));
        commands::execute_command(cli.command).await;
    }
}
