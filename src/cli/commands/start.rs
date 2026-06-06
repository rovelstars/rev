use crate::bus::protocol::{self, Message, MessageBody};
use tokio::net::UnixStream;

pub async fn run(service_name: &str) {
    let socket_path = crate::bus::socket_path();
    let stream = match UnixStream::connect(&socket_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rev: cannot connect to wirebus ({}): {}", socket_path.display(), e);
            std::process::exit(1);
        }
    };

    let (mut reader, mut writer) = stream.into_split();

    let msg = Message {
        id: 1,
        sender: "rev-cli".to_string(),
        auth_token: None,
        body: MessageBody::StartService {
            service: service_name.to_string(),
        },
    };

    if let Err(e) = protocol::send_message(&mut writer, &msg).await {
        eprintln!("rev: failed to send command: {}", e);
        std::process::exit(1);
    }

    match protocol::recv_message(&mut reader).await {
        Ok(response) => match response.body {
            MessageBody::Ok { message } => println!("{}", message),
            MessageBody::Error { message } => {
                eprintln!("rev: {}", message);
                std::process::exit(1);
            }
            _ => eprintln!("rev: unexpected response"),
        },
        Err(e) => {
            eprintln!("rev: failed to read response: {}", e);
            std::process::exit(1);
        }
    }
}
