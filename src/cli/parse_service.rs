// fn parse_service_id
// takes service name in these formats, and returns back (app_id, service, file_path) - only if its valid, otherwise throw error.
// com.rovelstars.files -> INVALID (no service name in app_id)
// com.rovelstars.files/indexer -> (com.rovelstars.files, indexer, com.rovelstars.files/indexer.rsc)
// com.rovelstars.files/indexer.rsc -> (com.rovelstars.files, indexer, com.rovelstars.files/indexer.rsc)
// com.rovelstars -> INVALID (no app name in app_id)
// com.rovelstars/files/indexer -> INVALID (no app name in app_id)
// com.rovelstars.files/ -> INVALID (no service name)
// com.rovelstars.files/.rsc -> INVALID (no service name)
// com.rovelstars.files/indexer.txt -> INVALID (invalid file extension)
// com.rovelstars.files/backup/cloud-service-a -> (com.rovelstars.files, backup/cloud-service-a, com.rovelstars.files/backup/cloud-service-a.rsc)
// files/indexer -> INVALID (no app id)

// returns (app_id, service, file_path) if valid, otherwise Error

use std::path::PathBuf;

pub fn parse_service(service_name: &str) -> Result<(String, String, PathBuf), String> {
    let parts: Vec<&str> = service_name.split('/').collect();
    if parts.len() < 2 {
        return Err("Invalid service name format".to_string());
    }

    let app_id = parts[0];
    if app_id.split('.').count() < 3 {
        return Err("Invalid app ID format".to_string());
    }

    let service_parts = &parts[1..];
    let service = service_parts.join("/");
    if service.is_empty() {
        return Err("Service name cannot be empty".to_string());
    }

    if let Some(ext) = std::path::Path::new(&service).extension() {
        if ext != "rsc" {
            return Err("Invalid file extension".to_string());
        }
    }

    let file_path = if service.ends_with(".rsc") {
        if service == ".rsc" || service.ends_with("/.rsc") {
            return Err("Service name cannot be empty".to_string());
        }
        PathBuf::from(format!("{}/{}", app_id, service))
    } else {
        PathBuf::from(format!("{}/{}.rsc", app_id, service))
    };

    Ok((app_id.to_string(), service, file_path))
}

#[cfg(test)]
mod tests {
    use super::parse_service;

    #[test]
    fn test_parse_service_name() {
        let cases = vec![
            ("com.rovelstars.files", None),
            (
                "com.rovelstars.files/indexer",
                Some((
                    "com.rovelstars.files",
                    "indexer",
                    "com.rovelstars.files/indexer.rsc",
                )),
            ),
            (
                "com.rovelstars.files/indexer.rsc",
                Some((
                    "com.rovelstars.files",
                    "indexer.rsc",
                    "com.rovelstars.files/indexer.rsc",
                )),
            ),
            ("com.rovelstars", None),
            ("com.rovelstars/files/indexer", None),
            ("com.rovelstars.files/", None),
            ("com.rovelstars.files/.rsc", None),
            ("com.rovelstars.files/indexer.txt", None),
            (
                "com.rovelstars.files/backup/cloud-service-a",
                Some((
                    "com.rovelstars.files",
                    "backup/cloud-service-a",
                    "com.rovelstars.files/backup/cloud-service-a.rsc",
                )),
            ),
            ("files/indexer", None),
        ];

        for (input, expected) in cases {
            let result = parse_service(input);
            match expected {
                Some((app_id, service, file_path)) => {
                    let (res_app_id, res_service, res_file_path) =
                        result.expect("Expected valid result");
                    assert_eq!(res_app_id, app_id);
                    assert_eq!(res_service, service);
                    assert_eq!(res_file_path.to_str().unwrap(), file_path);
                }
                None => {
                    assert!(
                        result.is_err(),
                        "Expected error for input: {}, but got Ok",
                        input
                    );
                }
            }
        }
    }
}
