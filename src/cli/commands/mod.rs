mod create;
mod read;
mod start;

//create Commands enum
#[derive(Debug, clap::Subcommand)]
pub enum Commands {
    Start { service_name: String },
    RunAsInit,
    Create { service_name: String },
    Read { service_name: String },
}

pub async fn execute_command(command: Commands) {
    match command {
        Commands::Create { service_name } => {
            create::run(&service_name);
        }
        Commands::Start { service_name } => {
            start::run(&service_name);
        }
        Commands::RunAsInit => {
            crate::init::run().await;
        }
        Commands::Read { service_name } => {
            read::run(&service_name);
        }
    }
}
