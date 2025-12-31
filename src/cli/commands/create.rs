use crate::cli::parse_service::parse_service;
use crate::parser::{ServiceConfig, serialize_service_config};
use std::{fs, path};
pub fn run(service_name: &String) {
    let (app_id, _service, file) =
        parse_service(service_name).expect("Invalid service name format.");
    let service_dir = path::PathBuf::from(format!("./Services/{}", app_id));
    println!("Creating {:?}\n{:?}\n", service_dir, file);
    fs::create_dir_all(&service_dir).expect("Failed to create service directory");
    let service_config = ServiceConfig {
        Name: service_name.clone(),
        ExecStart: "/usr/bin/python /home/ren/test-service.py".to_string(),
        // Env: [
        //     ("LD_LIBRARY_PATH", "/usr/local/lib/"),
        //     ("PKG_CONFIG_PATH", "/usr/local/lib/pkgconfig"),
        // ]
        // .into(),
        //WorkingDir: Some(path::PathBuf::from("/")),
        //Schedule: CronStr("*/5 * * * *".to_string()).into(),
        ..Default::default()
    };

    let buf = serialize_service_config(&service_config);
    let deserialized: ServiceConfig = rmp_serde::from_slice(&buf).unwrap();
    println!("\nDeserialized: {:?}", deserialized);

    let serialized_config = serialize_service_config(&service_config);
    //rsc = rev/rovelstars/runixOS service config
    let service_file_path = service_dir.join(file.file_name().unwrap());
    fs::write(&service_file_path, serialized_config).expect("Failed to write service config file");
    //serialize default service config to MessagePack format
    print!("\nCreated {:?}\n", service_name);
}
