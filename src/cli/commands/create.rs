use crate::parser::{CronStr, ServiceConfig, serialize_service_config};
use std::{fs, path};
use crate::cli::dir_rule::parse_service_name;
pub fn run(service_name: &String) {
    let (app_id, file) = parse_service_name(service_name)
        .expect("Failed to resolve service path");
    let service_dir = path::PathBuf::from(format!("./Services/{}", app_id.display()));
    println!("Creating {:?}\n{:?}", service_dir, file);
    fs::create_dir_all(&service_dir).expect("Failed to create service directory");
    let service_config = ServiceConfig {
        Name: service_name.clone(),
        Exec: path::PathBuf::from("/usr/local/bin/cz-louvre-default"),
        //LD_LIBRARY_PATH=/usr/local/lib/ PKG_CONFIG_PATH=/usr/local/lib/pkgconfig
        Env: [
            ("LD_LIBRARY_PATH".to_string(), "/usr/local/lib/".to_string()),
            (
                "PKG_CONFIG_PATH".to_string(),
                "/usr/local/lib/pkgconfig".to_string(),
            ),
        ]
        .iter()
        .cloned()
        .collect(),
        WorkingDir: Some(path::PathBuf::from("/")),
        Schedule: CronStr("*/5 * * * *".to_string()).into(),
        ..Default::default()
    };

    let mut buf = Vec::new();
    buf = serialize_service_config(&service_config);
    let deserialized: ServiceConfig = rmp_serde::from_slice(&buf).unwrap();
    println!("Deserialized: {:?}", deserialized);

    let serialized_config = serialize_service_config(&service_config);
    //rsc = rev/rovelstars/runixOS service config
    let service_file_path = format!("{}/{}.rsc", service_dir.display(), file);
    fs::write(&service_file_path, serialized_config).expect("Failed to write service config file");
    //serialize default service config to MessagePack format
    print!("Created {:?}\n", service_name);
}
