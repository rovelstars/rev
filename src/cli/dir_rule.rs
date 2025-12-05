use std::path::PathBuf;

/// Parse service name into directory path and filename
pub fn parse_service_name(input: &str) -> Result<(PathBuf, String), String> {
    let (appid, func_path) = split_appid_and_path(input)?;
    validate_appid(appid)?;
    Ok(build_service_path(appid, func_path))
}

/// Split string into APPID and optional function path
fn split_appid_and_path(input: &str) -> Result<(&str, Option<&str>), String> {
    if let Some(pos) = input.find('/') {
        let (a, b) = input.split_at(pos);
        let b = &b[1..]; // remove slash
        Ok((a, Some(b)))
    } else {
        Ok((input, None))
    }
}

/// final validation rule:
/// - One-part APPID is OK
/// - 3+ part Fully-Qualified App ID is OK
/// - 2-part Fully-Qualified App ID is INVALID
fn validate_appid(appid: &str) -> Result<(), String> {
    if appid.is_empty() {
        return Err("AppID cannot be empty".into());
    }

    let parts: Vec<&str> = appid.split('.').collect();

    match parts.len() {
        1 => {
            // simple name: must be alphanumeric or _
            let ok = parts[0]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ok {
                return Err(format!("Invalid AppID '{appid}': must be alphanumeric"));
            }
            Ok(())
        }

        2 => Err(format!(
            "Invalid AppID '{appid}': Fully-Qualified AppID must have at least 3 parts"
        )),

        3.. => {
            for p in &parts {
                if p.is_empty() {
                    return Err(format!("Invalid AppID '{appid}': empty segment"));
                }
                if !p.chars().all(|c| c.is_ascii_alphanumeric()) {
                    return Err(format!(
                        "Invalid AppID segment '{p}' in '{appid}' (must be alphanumeric)"
                    ));
                }
            }
            Ok(())
        }

        _ => unreachable!(),
    }
}

/// Build service directory + filename
///
/// Examples:
/// com.rovelstars.files/indexer  → rovelstars.com/files/indexer.rsc
/// myapp/scan                    → myapp/scan.rsc
/// index                         → index.rsc
fn build_service_path(appid: &str, func_path: Option<&str>) -> (PathBuf, String) {
    let mut service_dir = PathBuf::new();

    if appid.contains('.') {
        // reverse the dot segments
        let mut parts: Vec<&str> = appid.split('.').collect();
        parts.reverse();

        // first two segments become vendor.tld
        let tld = parts[1];
        let vendor = parts[0];
        service_dir.push(format!("{}.{}", vendor, tld));

        // remaining segments become subdirectories
        for p in parts.iter().skip(2) {
            service_dir.push(p);
        }
    } else {
        service_dir.push(appid);
    }

    match func_path {
        Some(path) => {
            let parts: Vec<&str> = path.split('/').collect();

            // all except last are directories
            for p in &parts[..parts.len() - 1] {
                service_dir.push(p);
            }

            // last becomes filename
            let filename = format!("{}.rsc", parts.last().unwrap());
            (service_dir, filename)
        }

        None => {
            // no path, filename from appid
            let filename = format!("{}.rsc", appid);
            (service_dir, filename)
        }
    }
}
