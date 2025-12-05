// create a D-bus like our own IPC bus system for inter-service communication, which uses Unix domain sockets under the hood.
//we call it R-Bus (Our Bus/RunixOS Bus/Rev Bus)
//we use tokio for async IO and serde for serialization

use tokio::net::UnixStream;

pub fn create(){
  
}