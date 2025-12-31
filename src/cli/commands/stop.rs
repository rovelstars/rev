use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub async fn run(service_name: &String) {
    let socket_path = "./rev-init.sock";
    let mut stream = UnixStream::connect(socket_path)
        .await
        .expect("Failed to connect to init server");
    let command = format!("stop {}\n", service_name);
    stream
        .write_all(command.as_bytes())
        .await
        .expect("Failed to send stop command");

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .await
        .expect("Failed to read response from init server");
    print!("{}", response);
    print!("Stopped {:?}\n", service_name);
}
