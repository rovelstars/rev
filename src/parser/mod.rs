use chrono::{DateTime, Utc, serde::ts_seconds_option};
#[allow(dead_code)]
use croner::Cron;
use rmp_serde::{Deserializer, Serializer};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

impl FromStr for CronStr {
    type Err = croner::errors::CronError; // or whatever error type croner uses

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Cron::from_str(s)?; // Validate
        Ok(CronStr(s.to_string()))
    }
}

impl std::fmt::Debug for CronStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "\"{}\"", self.0)
    }
}

/// A wrapper for a cron string that validates on deserialization.
#[derive(Clone, PartialEq, Eq)]
pub struct CronStr(pub String);

impl serde::Serialize for CronStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for CronStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        // Validate the cron string using croner
        Cron::from_str(&s).map_err(serde::de::Error::custom)?;
        Ok(CronStr(s))
    }
}

impl fmt::Display for CronStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    OnFailure,
    #[default]
    Never,
    OnResourceChange,
}

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

// ServiceInfo holds runtime information about a service such as its current status, PID, last exit code, and uptime.
#[allow(non_snake_case)]
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct ServiceInfo {
    pub Name: String,
    // IsRunning is 0 if not defined
    #[serde(default)]
    pub IsRunning: bool,
    #[serde(default)]
    pub Pid: Option<u32>,
    #[serde(default)]
    pub LastExitCode: Option<i32>,
    #[serde(with = "ts_seconds_option", default)]
    pub UpTimestamp: Option<DateTime<Utc>>, // unix timestamp when service was started. if empty, service is not running
    //link to service config
    pub Config: ServiceConfig,
}

// ServiceConfig holds the configuration details for a service.
#[allow(non_snake_case)]
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct ServiceConfig {
    pub Name: String,
    pub ExecStart: String,
    #[serde(default)]
    pub ExecStop: Option<String>,
    #[serde(default)]
    pub ExecReload: Option<String>,
    #[serde(default)]
    pub ExecStartPre: Option<String>,
    #[serde(default)]
    pub ExecStartPost: Option<String>,
    #[serde(default)]
    pub ExecStopPre: Option<String>,
    #[serde(default)]
    pub ExecStopPost: Option<String>,
    #[serde(default)]
    pub Env: EnvMap,
    #[serde(default)]
    pub WorkingDir: Option<std::path::PathBuf>,
    pub RestartPolicy: RestartPolicy,
    #[serde(default)]
    pub TimeoutStop: Option<u64>, // in seconds
    // cron schedule string, e.g. "0 5 * * *" for daily at 5am, for every 5 minutes use "*/5 * * * *"
    #[serde(default)]
    pub Schedule: Option<CronStr>,
    // if service was already running, don't restart it for scheduled runs - leave it be.
    #[serde(default)]
    pub ForceRestartOnSchedule: bool,
}

#[allow(non_snake_case)]
#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub DisabledServices: Vec<String>,
    pub EnabledServices: Vec<String>,
    pub Services: HashMap<String, ServiceConfig>,
}

pub fn serialize_service_config(service_config: &ServiceConfig) -> Vec<u8> {
    let mut buf = Vec::new();
    service_config
        .serialize(&mut Serializer::new(&mut buf))
        .expect("Serialization failed");
    buf
}

pub fn deserialize_service_config(data: &[u8]) -> ServiceConfig {
    let mut de = Deserializer::new(data);
    ServiceConfig::deserialize(&mut de).expect("Deserialization failed")
}
