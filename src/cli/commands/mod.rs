mod create;
mod read;
mod start;
mod stop;

//create Commands enum
#[derive(Debug, clap::Subcommand)]
pub enum Commands {
    Start {
        service_name: String,
    },
    RunAsInit {
      // auto_start cant be bool because clap doesnt support bool args, and makes them required flags.
        #[arg(default_value_t = 1)]
        auto_start: u8,
    },
    Create {
        service_name: String,
    },
    Read {
        service_name: String,
    },
    Stop {
        service_name: String,
    },
}

pub async fn execute_command(command: Commands) {
    match command {
        Commands::Create { service_name } => {
            create::run(&service_name);
        }
        Commands::Start { service_name } => {
            start::run(&service_name).await;
        }
        Commands::RunAsInit { auto_start } => {
            crate::init::run(auto_start == 1).await;
        }
        Commands::Read { service_name } => {
            read::run(&service_name);
        }
        Commands::Stop { service_name } => {
            stop::run(&service_name).await;
        }
    }
}
