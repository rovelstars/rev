use crate::parser::deserialize_service_config;
use std::fs;
use crate::cli::dir_rule::parse_service_name;
pub fn run(service_name: &String) {
    let (service_dir, file) = parse_service_name(service_name)
        .expect("Failed to resolve service path");
    //rsc = rev/rovelstars/runixOS service config
    let service_file_path = format!("{}/{}.rsc", service_dir.display(), file);
    print!("Reading {:?}\n", service_name);
    let data = fs::read(&service_file_path).expect("Failed to read service config file");
    let serialized_config = deserialize_service_config(&data);
    println!("Service Config: {:?}\n", serialized_config);
}
