use crate::cli::parse_service::parse_service;
use std::{fs, path};

pub fn run(service_name: &str) {
    let (app_id, _service, file) = parse_service(service_name)
        .expect("Invalid service name format. Expected format: com.example.app/service-name");
    let service_dir = path::PathBuf::from(format!("./Services/{}", app_id));
    let service_file_path = match file.file_name() {
        Some(name) => service_dir.join(name),
        None => {
            eprintln!("rev: invalid file path for service");
            std::process::exit(1);
        }
    };

    let text = fs::read_to_string(&service_file_path).expect("Failed to read service config file");

    // Validate it parses
    let config: crate::parser::ServiceConfig =
        toml::from_str(&text).expect("Failed to parse service config");

    println!("# {}\n", service_name);
    println!("{}", text);
    println!("# Parsed: {:?}", config);
}
