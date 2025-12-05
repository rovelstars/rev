mod cli;
mod dashboard;
mod init;
mod parser;
mod service;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let invocation_name = args
        .get(0)
        .map(|s| s.as_str())
        .unwrap_or("")
        .split(std::path::MAIN_SEPARATOR)
        .last()
        .unwrap_or("");
    if invocation_name.eq("init") || std::process::id() == 1 {
        init::run().await;
    } else {
        cli::run(&args).await;
    }
}
