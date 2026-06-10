use chrono::{DateTime, Utc, serde::ts_seconds_option};
#[allow(dead_code)]
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

// ---------------------------------------------------------------------------
// Service directory helpers
// ---------------------------------------------------------------------------

/// Returns the list of directories to scan for .rsc service files.
pub fn service_dirs() -> Vec<PathBuf> {
    if cfg!(debug_assertions) {
        vec![PathBuf::from("./Services")]
    } else {
        vec![
            PathBuf::from("/Core/Services"),
            PathBuf::from("/Core/UserServices"),
            PathBuf::from("/Construct/Services"),
            // Per-user services are handled separately via /Space/*/.Services
        ]
    }
}

// ---------------------------------------------------------------------------
// CronStr — validated cron expression wrapper
// ---------------------------------------------------------------------------

/// A wrapper for a cron string that validates on deserialization.
#[derive(Clone, PartialEq, Eq)]
pub struct CronStr(pub String);

impl FromStr for CronStr {
    type Err = croner::errors::CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Cron::from_str(s)?; // validate
        Ok(CronStr(s.to_string()))
    }
}

impl fmt::Debug for CronStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "\"{}\"", self.0)
    }
}

impl fmt::Display for CronStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serialize for CronStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CronStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Cron::from_str(&s).map_err(serde::de::Error::custom)?;
        Ok(CronStr(s))
    }
}

// ---------------------------------------------------------------------------
// RestartPolicy
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    #[default]
    Never,
    OnResourceChange,
}

// ---------------------------------------------------------------------------
// EnvMap
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EnvMap(pub HashMap<String, String>);

impl<K: ToString, V: ToString> FromIterator<(K, V)> for EnvMap {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        EnvMap(
            iter.into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }
}

impl<K: ToString, V: ToString, const N: usize> From<[(K, V); N]> for EnvMap {
    fn from(arr: [(K, V); N]) -> Self {
        arr.into_iter().collect()
    }
}

impl EnvMap {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::ops::Deref for EnvMap {
    type Target = HashMap<String, String>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for EnvMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'a> IntoIterator for &'a EnvMap {
    type Item = (&'a String, &'a String);
    type IntoIter = std::collections::hash_map::Iter<'a, String, String>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

// ---------------------------------------------------------------------------
// ServiceConfig — persisted as TOML in .rsc files
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct ServiceConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub exec_start: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_stop: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_reload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_start_pre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_start_post: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_stop_pre: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_stop_post: Option<String>,
    #[serde(default, skip_serializing_if = "EnvMap::is_empty")]
    pub env: EnvMap,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    /// WireBus names this service provides. When a Lookup for one of these
    /// names misses on the Highway, rev starts this service (bus-activation)
    /// and waits for it to register the name before answering the Lookup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_stop: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronStr>,
    #[serde(default)]
    pub force_restart_on_schedule: bool,
}

// ---------------------------------------------------------------------------
// ServiceInfo — runtime state (serialized over IPC via MessagePack)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct ServiceInfo {
    pub name: String,
    #[serde(default)]
    pub is_running: bool,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(with = "ts_seconds_option", default)]
    pub up_timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub cpu_seconds: Option<f64>,
    #[serde(default)]
    pub tasks: Option<u32>,
    #[serde(default)]
    pub config_path: Option<String>,
    pub config: ServiceConfig,
}

impl ServiceInfo {
    /// Read /proc/<pid>/status and /proc/<pid>/stat to get live resource usage.
    #[allow(dead_code)]
    pub fn refresh_proc_stats(&mut self) {
        let pid = match self.pid {
            Some(p) if self.is_running => p,
            _ => return,
        };
        // Memory: VmRSS from /proc/<pid>/status
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", pid)) {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    if let Some(kb) = line.split_whitespace().nth(1).and_then(|s| s.parse::<u64>().ok()) {
                        self.memory_bytes = Some(kb * 1024);
                    }
                }
                if line.starts_with("Threads:") {
                    if let Some(t) = line.split_whitespace().nth(1).and_then(|s| s.parse::<u32>().ok()) {
                        self.tasks = Some(t);
                    }
                }
            }
        }
        // CPU: utime + stime from /proc/<pid>/stat (fields 14,15 in clock ticks)
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
            let fields: Vec<&str> = stat.split_whitespace().collect();
            if fields.len() > 14 {
                let utime = fields[13].parse::<u64>().unwrap_or(0);
                let stime = fields[14].parse::<u64>().unwrap_or(0);
                let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
                if ticks_per_sec > 0 {
                    self.cpu_seconds = Some((utime + stime) as f64 / ticks_per_sec as f64);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TOML serialization for .rsc files
// ---------------------------------------------------------------------------

pub fn serialize_service_config(config: &ServiceConfig) -> Result<String, toml::ser::Error> {
    toml::to_string_pretty(config)
}

pub fn deserialize_service_config(data: &str) -> Result<ServiceConfig, toml::de::Error> {
    toml::from_str(data)
}
