use crate::cli::parse_service::parse_service;
use crate::parser::deserialize_service_config;
use std::fs;
use std::path;
pub fn run(service_name: &String) {
    let (app_id, _service, file) = parse_service(service_name)
        .expect("Invalid service name format. Expected format: com.example.app/service-name");
    let service_dir = path::PathBuf::from(format!("./Services/{}", app_id));
    let service_file_path = service_dir.join(file.file_name().unwrap());
    print!("Reading {:?}\n", service_name);
    let data = fs::read(&service_file_path).expect("Failed to read service config file");
    let serialized_config = deserialize_service_config(&data);
    println!("Service Config: {:?}\n", serialized_config);
}
