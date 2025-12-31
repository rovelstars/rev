use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use crate::cli::parse_service::parse_service;
use crate::init::services;
use crate::service::start_service_from_path;
pub async fn run() -> tokio::io::Result<()> {
    let socket_path = "./rev-init.sock";
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;

    println!("Init server listening on {}", socket_path);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_client(stream));
    }
}

async fn handle_client(stream: UnixStream) -> tokio::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = buf_reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            break; // Connection closed
        }

        let command = line.trim();
        let parts: Vec<&str> = command.split_whitespace().collect();
        if parts.is_empty() {
            continue;
        }

        match parts[0] {
            "start" if parts.len() == 2 => {
                let service_name = parts[1];
                start_service(service_name).await;
                writer
                    .write_all(format!("Started service: {}\n", service_name).as_bytes())
                    .await?;
            }
            "stop" if parts.len() == 2 => {
                let service_name = parts[1];
                stop_service(service_name).await;
                writer
                    .write_all(format!("Stopped service: {}\n", service_name).as_bytes())
                    .await?;
            }
            "exit" => {
                writer.write_all(b"Goodbye!\n").await?;
                break;
            }
            _ => {
                writer
                    .write_all(b"Unknown command. Use 'start <service>', 'stop <service>', or 'exit'.\n")
                    .await?;
            }
        }
    }

    Ok(())
}

async fn start_service(name: &str) {
    println!("(Would start service: {})", name);
    let (app_id, _service, file) = parse_service(name)
        .expect("Invalid service name format. Expected format: com.example.app/service-name");
    let service_dir = std::path::PathBuf::from(format!("./Services/{}", app_id));
    start_service_from_path(&service_dir.join(file.file_name().unwrap()));
}
async fn stop_service(name: &str) {
    println!("(Would stop service: {})", name);
    services::get_service(name).map(|service_info| {
        if let Some(pid) = service_info.Pid {
            // Send SIGTERM to the process
            let pid_i32 = pid as i32;
            unsafe {
                let _ = libc::kill(pid_i32, libc::SIGTERM);
            }
        }
    });
}
