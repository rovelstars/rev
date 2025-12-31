use clap::Parser;
pub mod parse_service;
mod commands;
use crate::dashboard;

#[derive(Parser, Debug)]
#[command(name = "rev")]
#[command(about = "RunixOS Service Manager", long_about = None)]
struct Cli {
    //make these following:
    //rev [start|stop|status|enable|disable] <service_name>
    #[command(subcommand)]
    command: commands::Commands,
}

pub async fn run(args: &[String]) {
    //if no args, show ratatui dashboard, otherwise parse args with clap
    println!("Args: {:?}", args);
    if args.len() == 1 {
        // Show ratatui dashboard
        println!("Showing ratatui dashboard...");
        let _ = dashboard::show();
    } else {
        // Parse args with clap
        let cli = Cli::parse_from(args.iter().map(|s| s.as_str()));
        commands::execute_command(cli.command).await;
    }
}
