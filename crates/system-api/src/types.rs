// SPDX-License-Identifier: GPL-3.0-only
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerSample {
    pub sampled_at_ms: u64,
    pub interval_ms: u16,
    pub cpu_milliwatts: Option<f64>,
    pub gpu_milliwatts: Option<f64>,
    pub ane_milliwatts: Option<f64>,
    pub thermal_pressure: Option<String>,
    pub source: String,
    pub scope: String,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_time: u64,
    pub exe: String,
    pub uid: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessInfo {
    pub identity: ProcessIdentity,
    pub name: String,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub parent_pid: Option<u32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentity {
    pub label: String,
    pub domain: String,
    pub uid: u32,
    pub pid: Option<u32>,
    pub process_start_time: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_sid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_hash: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceInfo {
    pub identity: ServiceIdentity,
    pub pid: Option<u32>,
    pub last_exit: Option<i32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    pub received_bytes: u64,
    pub transmitted_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: String,
    pub details: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SystemSnapshot {
    pub sampled_at_ms: u64,
    pub source: String,
    pub processes: Vec<ProcessInfo>,
    pub services: Vec<ServiceInfo>,
    pub network: Vec<NetworkInterface>,
    pub devices: Vec<DeviceInfo>,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogQuery {
    pub process: Option<String>,
    pub contains: Option<String>,
    pub seconds: u32,
    pub limit: usize,
}

impl Default for LogQuery {
    fn default() -> Self {
        Self {
            process: Some("glass-terminal".into()),
            contains: None,
            seconds: 60,
            limit: 100,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub process: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "target", deny_unknown_fields)]
pub enum PrivilegedAction {
    StopProcess(ProcessIdentity),
    RestartUserService(ServiceIdentity),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedAction {
    pub ticket: String,
    pub summary: String,
    pub expires_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationResult {
    pub success: bool,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditRecord {
    pub timestamp_ms: u64,
    pub caller_uid: u32,
    pub caller_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_sid: Option<String>,
    pub operation: String,
    pub target: String,
    pub result: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerSessionIdentity {
    pub broker_uid: u32,
    pub broker_pid: u32,
    pub caller_uid: u32,
    pub caller_pid: u32,
    pub administrator: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker_sid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_sid: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRecord {
    pub id: String,
    pub path: String,
    pub before_sha256: Option<String>,
    pub after_sha256: String,
    pub status: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManagedFile {
    pub path: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCapabilities {
    pub write_workspace: bool,
    pub network: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginRun {
    pub started_at_ms: u64,
    pub program: String,
    pub exit_code: Option<i32>,
    pub output: String,
    pub sandbox: String,
    pub broker_access: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEvent {
    pub sampled_at_ms: u64,
    pub source: String,
    pub kind: String,
    pub paths: Vec<String>,
}

// FVPROV1 5063 W3siYm9keSI6eyJhY3Rvcl9rZXlfaWQiOiI5ZjAwOTVhYmY4ODEwZGQwY2Y4NTdlZDBlNDI0ODg0NmRlMmFkMDc2ZmZhYTY2MWIzNmM5Yzc3YWFjOGVlOWMzIiwiYWxnb3JpdGhtIjoiRWQyNTUxOSIsImF1dGhvciI6IkoteC0gWiIsImNvbnRlbnRfc2hhMjU2IjoiNGYxZjYwYjhlNGE1Zjg1M2Y0MmNkOTFmN2ZhMDlkNDViNjMyMWNiZWMxYzRiNWE2YWJlNmNjYmQwNjQxN2ZjOSIsImNvcHlfaWQiOiJ2MC4xLjAtYmV0YS4xIiwiY3JlYXRlZF9hdCI6IjIwMjYtMTEtMTgiLCJlZGl0aW9uIjoiYmV0YSIsImlzc3Vlcl9wb2xpY3kiOm51bGwsImtpbmQiOiJjcmVhdGUiLCJtb2RlIjoic2VsZl9zaWduIiwicGFyZW50X3ZlcnNpb24iOm51bGwsInByZXZpb3VzX3JlY29yZF9zaGEyNTYiOm51bGwsInNjaGVtYSI6ImZpbGV2ZWlsLnByb3ZlbmFuY2Uuc2lnbmF0dXJlLnYyIiwic2VxdWVuY2UiOjAsInRyYW5zZmVyX3RvX2tleV9pZCI6bnVsbCwidmVyc2lvbiI6InYwLjEuMC1iZXRhLjEiLCJ3b3JrX2lkIjoib3JnLnBseXJhLnRlcm1pbmFsIn0sInNpZ25hdHVyZXMiOlt7ImtleV9pZCI6IjlmMDA5NWFiZjg4MTBkZDBjZjg1N2VkMGU0MjQ4ODQ2ZGUyYWQwNzZmZmFhNjYxYjM2YzljNzdhYWM4ZWU5YzMiLCJwdWJsaWNfa2V5X2hleCI6ImMzZTkyZDhkZDNkZWM4NTliODdlOTY4MjIyYjI4OWJhNzg5M2NjOGM4NWZjOTE2MjI2YjZiNjI5NjFiOTU5ZDciLCJzaWduYXR1cmVfaGV4IjoiMTNhYjdiMGNiODdkYWExM2JkZmJkNDJlMDM3MWZhZjNmM2U1NmUzNmMzZjYwZTA5MTZhYzdlOWU4ZDU1YmVhNTJkOTg4Y2ViMDA4MzVjNWNlYzZhNjVkMTU3YmZiYTU3ZjIyZjE1MjE3N2IzOTE0OWQwYTNhZjI4MDI5N2FiMDUifV19XQ== FVPROV1-END
