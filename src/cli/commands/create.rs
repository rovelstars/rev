use crate::cli::parse_service::parse_service;
use crate::parser::{ServiceConfig, serialize_service_config};
use std::{fs, path};

pub fn run(service_name: &str) {
    let (app_id, _service, file) =
        parse_service(service_name).expect("Invalid service name format.");
    let service_dir = path::PathBuf::from(format!("./Services/{}", app_id));
    let service_file_path = match file.file_name() {
        Some(name) => service_dir.join(name),
        None => {
            eprintln!("rev: invalid file path for service");
            std::process::exit(1);
        }
    };

    if service_file_path.exists() {
        eprintln!("Service '{}' already exists at {:?}", service_name, service_file_path);
        std::process::exit(1);
    }

    fs::create_dir_all(&service_dir).expect("Failed to create service directory");

    let config = ServiceConfig {
        name: service_name.to_string(),
        exec_start: "/usr/bin/echo hello".to_string(),
        ..Default::default()
    };

    let toml_str = serialize_service_config(&config).expect("Failed to serialize config");
    fs::write(&service_file_path, &toml_str).expect("Failed to write service config file");

    println!("Created service '{}' at {}", service_name, service_file_path.display());
    println!("Edit the file to configure your service:\n");
    println!("{}", toml_str);
}
