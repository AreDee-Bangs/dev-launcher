use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlAction {
    StopWorkspace,
    RestartWorkspace,
    RestartService { service: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub id: String,
    pub issued_at_ms: u64,
    pub action: ControlAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub id: String,
    pub ok: bool,
    pub message: String,
    pub completed_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRuntimeSnapshot {
    pub workspace_hash: String,
    pub worker_pid: u32,
    pub detached: bool,
    pub updated_at_ms: u64,
    /// Port offset chosen dynamically for this run (added to base ports).
    /// Older snapshots that predate this field deserialise to 0.
    #[serde(default)]
    pub port_offset: u16,
    pub services: Vec<ServiceRuntimeSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceRuntimeSnapshot {
    pub name: String,
    pub health: String,
    pub pid: Option<u32>,
    pub url: Option<String>,
    pub log_path: String,
    pub startup_secs: u64,
    pub diagnosis: Option<String>,
}

pub fn status_path(slug: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/dev-launcher-{slug}.status.json"))
}

pub fn request_path(slug: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/dev-launcher-{slug}.control-request.json"))
}

pub fn response_path(slug: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/dev-launcher-{slug}.control-response.json"))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn clear_runtime_files(slug: &str) {
    let _ = fs::remove_file(status_path(slug));
    let _ = fs::remove_file(request_path(slug));
    let _ = fs::remove_file(response_path(slug));
}

pub fn publish_snapshot(slug: &str, snapshot: &WorkspaceRuntimeSnapshot) -> io::Result<()> {
    write_json_atomic(&status_path(slug), snapshot)
}

pub fn read_snapshot(slug: &str) -> Option<WorkspaceRuntimeSnapshot> {
    read_json::<WorkspaceRuntimeSnapshot>(&status_path(slug)).ok()
}

pub fn queue_request(slug: &str, action: ControlAction) -> io::Result<ControlRequest> {
    let req_path = request_path(slug);
    if req_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "another workspace operation is already pending",
        ));
    }

    let _ = fs::remove_file(response_path(slug));

    let req = ControlRequest {
        id: format!("{slug}-{}-{}", now_ms(), std::process::id()),
        issued_at_ms: now_ms(),
        action,
    };
    write_json_atomic(&req_path, &req)?;
    Ok(req)
}

pub fn take_request(slug: &str) -> Option<ControlRequest> {
    let path = request_path(slug);
    let req = read_json::<ControlRequest>(&path).ok()?;
    let _ = fs::remove_file(path);
    Some(req)
}

pub fn publish_response(slug: &str, response: &ControlResponse) -> io::Result<()> {
    write_json_atomic(&response_path(slug), response)
}

pub fn wait_for_response(
    slug: &str,
    request_id: &str,
    timeout: Duration,
) -> io::Result<Option<ControlResponse>> {
    let deadline = std::time::Instant::now() + timeout;
    let path = response_path(slug);

    while std::time::Instant::now() < deadline {
        if let Ok(resp) = read_json::<ControlResponse>(&path) {
            if resp.id == request_id {
                return Ok(Some(resp));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    Ok(None)
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let tmp = temp_path(path);
    let body = serde_json::to_vec_pretty(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    fs::write(&tmp, body)?;
    fs::rename(tmp, path)?;
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "dev-launcher-control.json".to_string());
    path.with_file_name(format!("{name}.tmp-{}", std::process::id()))
}
