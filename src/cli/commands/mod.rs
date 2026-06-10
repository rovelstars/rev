mod create;
mod install;
mod read;
mod service_client;
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
    /// Validate a .rsc service file and install it: system-wide into
    /// /Construct/Services, or per-user (--user) into the account vault.
    Install {
        /// Path to the .rsc file to install.
        file: String,
        /// Install as a per-user service instead of system-wide.
        #[arg(long)]
        user: bool,
    },
    /// Run only the WireBus System Highway, without any init behaviour. A
    /// dev and benchmark helper: it brings up the bus server on the configured
    /// Highway socket and serves until killed. Hidden from normal help.
    #[command(hide = true)]
    BusServe {
        /// Mark this session id as the active seat session before serving, so a
        /// root (System) client can OpenDevice without going through a full
        /// StartSession. Dev/test only (used by the seat fd-pass VM harness).
        #[arg(long)]
        seat_session: Option<u64>,
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
        Commands::Install { file, user } => {
            install::run(&file, user);
        }
        Commands::BusServe { seat_session } => {
            let sock = crate::bus::socket_path();
            let sock = sock.to_string_lossy().to_string();
            if let Some(sid) = seat_session {
                crate::seat::set_active_session(sid);
                println!("rev: bus-serve: active seat session = {sid} (dev/test)");
            }
            println!("rev: bus-serve: System Highway on {sock} (dev/benchmark mode)");
            if let Err(e) =
                crate::bus::server::run(&sock, crate::bus::policy::Tier::Highway).await
            {
                eprintln!("rev: bus-serve: {e}");
            }
        }
    }
}
