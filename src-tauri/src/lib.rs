use chrono::{DateTime, Datelike, Local, Timelike, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt as _;
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader as StdBufReader, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
use tauri::{AppHandle, Emitter, Manager, State};
use uuid::Uuid;

const PROTOCOL_VERSION: &str = "1.0";
const VAULT_SERVICE: &str = "com.alfred.desktop";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn hide_windows_console(command: &mut Command) {
    #[cfg(windows)]
    {
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = command;
}

#[derive(Default)]
struct RuntimeState {
    native_host: Arc<Mutex<Option<NativeHostProcess>>>,
    run_controls: Arc<Mutex<HashMap<String, String>>>,
    /// One-step policy overrides granted by the user at the "waiting" prompt:
    /// run_id -> step_id. Lets an approved request_user step pass once, including
    /// unknown-effect steps that no permission grant could cover. hard_deny is
    /// never overridable.
    approved_overrides: Arc<Mutex<HashMap<String, String>>>,
    /// Mid-run user guidance queued by the cockpit's steer bar: run_id ->
    /// pending notes. The goal loop drains them into the planner's history.
    steer_notes: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

/// The host speaks newline-delimited JSON on stdio. A dedicated worker thread owns
/// the pipes so callers can time out a stuck request, kill the host, and recover
/// instead of blocking the automation runtime forever.
struct NativeHostProcess {
    child: Child,
    to_host: mpsc::Sender<String>,
    from_host: mpsc::Receiver<Result<String, String>>,
    capability_token: String,
    last_stderr: Arc<Mutex<String>>,
}

fn spawn_native_host(app: &AppHandle) -> Result<NativeHostProcess, String> {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut host_command = Command::new(native_host_executable(app)?);
    hide_windows_console(&mut host_command);
    let mut child = host_command
        .env("ALFRED_CAPABILITY_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Could not open native-host input.".to_string())?;
    let mut stdout = StdBufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| "Could not open native-host output.".to_string())?,
    );
    let last_stderr = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let sink = last_stderr.clone();
        std::thread::spawn(move || {
            let reader = StdBufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(mut held) = sink.lock() {
                    *held = line.chars().take(400).collect();
                }
            }
        });
    }
    let (to_host, worker_inbox) = mpsc::channel::<String>();
    let (worker_outbox, from_host) = mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        while let Ok(line) = worker_inbox.recv() {
            let result = (|| {
                writeln!(stdin, "{line}").map_err(|error| {
                    format!("Could not write to the Windows automation host: {error}")
                })?;
                stdin.flush().map_err(|error| {
                    format!("Could not flush the Windows automation host request: {error}")
                })?;
                let mut response = String::new();
                stdout.read_line(&mut response).map_err(|error| {
                    format!("Could not read the Windows automation host response: {error}")
                })?;
                if response.is_empty() {
                    return Err("The native host closed the connection.".into());
                }
                Ok(response)
            })();
            let failed = result.is_err();
            if worker_outbox.send(result).is_err() || failed {
                break;
            }
        }
    });
    Ok(NativeHostProcess {
        child,
        to_host,
        from_host,
        capability_token: token,
        last_stderr,
    })
}

impl Drop for NativeHostProcess {
    fn drop(&mut self) {
        // kill() alone leaves a zombie until wait(). Always reap.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    onboarding_complete: bool,
    provider: String,
    library_path: String,
    screenshot_retention: String,
    theme: String,
    /// Screenshots leave the machine when attached to a cloud planner CLI, so
    /// visual grounding can be disabled, but is on for new installations because
    /// it is the fallback for canvas-based and accessibility-poor applications.
    #[serde(default)]
    share_screenshots_with_planner: bool,
    /// Local JSONL of planner turns and tool calls. Off by default; never sent
    /// to a provider. Useful when diagnosing a failed run.
    #[serde(default)]
    diagnostic_logging: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            onboarding_complete: false,
            provider: "codex".into(),
            library_path: String::new(),
            screenshot_retention: "failures".into(),
            theme: "system".into(),
            share_screenshots_with_planner: true,
            diagnostic_logging: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    os: String,
    architecture: String,
    default_library_path: String,
    native_host: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    id: String,
    name: String,
    command: String,
    installed: bool,
    version: Option<String>,
    credential_stored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCommand {
    id: String,
    method: String,
    effect: String,
    intent: String,
    #[serde(default)]
    target_label: Option<String>,
    #[serde(default)]
    params: Value,
    #[serde(default)]
    run_id: Option<String>,
}

/// A state check evaluated against the target application (UIA element lookup for
/// native steps, DOM observation for browser steps). `absent` inverts the match,
/// e.g. "wait until the progress dialog is gone".
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StepCondition {
    #[serde(default)]
    automation_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    control_type: Option<String>,
    #[serde(default)]
    url_contains: Option<String>,
    #[serde(default)]
    absent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowStep {
    id: String,
    title: String,
    kind: String,
    effect: String,
    #[serde(default)]
    application: Option<String>,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    target_label: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default = "default_retries")]
    retries: u8,
    /// Polled before acting; the step fails if the condition is not met in time.
    #[serde(default)]
    wait_for: Option<StepCondition>,
    /// Checked before every attempt (an already-satisfied end state skips the
    /// action, which makes resume idempotent) and waited for after each action.
    #[serde(default)]
    expect: Option<StepCondition>,
    /// Name of a run-scoped variable that captures this step's result value.
    /// Later steps reference it as `${name}` inside payload strings, which is how
    /// data moves from one application to another.
    #[serde(default)]
    save_as: Option<String>,
}

fn default_timeout() -> u64 {
    30_000
}
fn default_retries() -> u8 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    id: String,
    name: String,
    goal: String,
    version: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    status: String,
    /// The brain used while the workflow was learned. Re-running a saved goal
    /// defaults to this provider, while still allowing the user to choose another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    planner_provider: Option<String>,
    required_apps: Vec<String>,
    steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequest {
    #[serde(default = "protocol_version")]
    protocol_version: String,
    #[serde(default)]
    run_id: String,
    #[serde(default)]
    workflow_step: String,
    application: String,
    intent: String,
    effect: String,
    target_label: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

fn protocol_version() -> String {
    PROTOCOL_VERSION.into()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDecision {
    decision: String,
    reason: String,
    rule: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionGrant {
    id: String,
    application: String,
    allowed_effects: Vec<String>,
    allowed_intents: Vec<String>,
    enabled: bool,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowSchedule {
    id: String,
    workflow_id: String,
    workflow_name: String,
    hour: u32,
    minute: u32,
    days: Vec<u32>,
    enabled: bool,
    last_triggered_key: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunCheckpoint {
    run_id: String,
    workflow_id: String,
    next_step_index: usize,
    status: String,
    error: Option<String>,
    updated_at: DateTime<Utc>,
}

/// Alfred-owned memory for a live goal. Provider sessions are useful context,
/// but never the source of truth: this ledger is atomically persisted after
/// every observation, plan, action, failure, and completion claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalRunMemory {
    schema_version: u32,
    run_id: String,
    provider: String,
    #[serde(default)]
    provider_session_id: Option<String>,
    goal: String,
    #[serde(default)]
    applications: Vec<String>,
    #[serde(default)]
    planner_turns: u32,
    #[serde(default)]
    provider_session_resets: u8,
    #[serde(default)]
    next_step_index: usize,
    #[serde(default)]
    pinned_browser_tab: Option<i64>,
    #[serde(default)]
    history: Vec<String>,
    #[serde(default)]
    working_plan: Vec<String>,
    #[serde(default)]
    consecutive_failures: u32,
    #[serde(default)]
    actions_since_check_in: u32,
    #[serde(default)]
    last_observation: String,
    #[serde(default)]
    pending_action: Option<WorkflowStep>,
    #[serde(default)]
    completion_claim: Option<String>,
    #[serde(default)]
    completion_evidence: Vec<String>,
    #[serde(default)]
    verification_attempts: u32,
    /// Most recent user-authored text that Alfred proved was entered into an
    /// application. External-publication goals use it as an outcome anchor: the
    /// same text must later appear in non-editable, current desktop state before
    /// a completion claim can pass.
    #[serde(default)]
    last_typed_text: Option<String>,
    #[serde(default)]
    last_typed_application: Option<String>,
    /// Live control name returned by the host/extension after the last action.
    #[serde(default)]
    last_resolved_label: Option<String>,
    /// Observation captured immediately before the last save-class action.
    #[serde(default)]
    save_baseline: Option<String>,
    /// Application that received the save action; used to pick the right title
    /// out of a multi-application observation.
    #[serde(default)]
    save_application: Option<String>,
    #[serde(default)]
    saw_publish_commit: bool,
    #[serde(default)]
    save_committed: bool,
    status: String,
    #[serde(default)]
    completion_summary: Option<String>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunEvent {
    run_id: String,
    sequence: usize,
    step_id: String,
    title: String,
    detail: String,
    application: String,
    status: String,
    progress: u8,
    evidence_data_url: Option<String>,
    timestamp: DateTime<Utc>,
}

#[derive(Debug)]
struct ProviderInvocation {
    command: String,
    args: Vec<String>,
    stdin: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderPlan {
    steps: Vec<ProviderPlanStep>,
}

#[cfg(test)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderPlanStep {
    #[serde(default)]
    title: String,
    application: String,
    #[serde(alias = "kind")]
    method: String,
    #[serde(default)]
    target_label: Option<String>,
    #[serde(default = "empty_json_object", alias = "payload")]
    params: Value,
}

#[cfg(test)]
fn empty_json_object() -> Value {
    serde_json::json!({})
}

fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("settings.json"))
}
fn permissions_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("permissions.json"))
}
fn schedules_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("schedules.json"))
}
fn checkpoints_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app_data_dir(app)?.join("checkpoints");
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn goal_run_memory_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    let path = app_data_dir(app)?.join("goal-runs");
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path.join(format!("{run_id}.json")))
}

fn save_goal_run_memory(app: &AppHandle, memory: &mut GoalRunMemory) -> Result<(), String> {
    memory.updated_at = Utc::now();
    write_json(&goal_run_memory_path(app, &memory.run_id)?, memory)
}

#[tauri::command]
fn get_goal_run_memory(app: AppHandle, run_id: String) -> Result<Option<GoalRunMemory>, String> {
    let path = goal_run_memory_path(&app, &run_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("write");
    let temporary = path.with_file_name(format!(
        ".{stem}.{}.tmp",
        Uuid::new_v4().simple()
    ));
    let written = fs::write(&temporary, contents);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    Ok(())
}

fn read_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T, String> {
    if !path.exists() {
        return Ok(T::default());
    }
    let contents = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&contents).map_err(|error| error.to_string())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let contents = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    atomic_write(path, &contents)
}

fn diagnostic_logging_enabled(app: &AppHandle) -> bool {
    get_settings(app.clone())
        .map(|settings| settings.diagnostic_logging)
        .unwrap_or(false)
}

fn run_logs_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app_data_dir(app)?.join("run-logs");
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn run_log_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    Ok(run_logs_dir(app)?.join(format!("{run_id}.jsonl")))
}

fn compact_log_value(value: &Value) -> Value {
    match value {
        Value::String(text) if text.len() > 400 => {
            Value::String(format!("{}…(+{} chars)", text.chars().take(200).collect::<String>(), text.len() - 200))
        }
        Value::Object(map) => {
            let mut compact = serde_json::Map::new();
            for (key, item) in map {
                if matches!(key.as_str(), "base64" | "dataUrl" | "capabilityToken") {
                    compact.insert(key.clone(), Value::String("[omitted]".into()));
                } else {
                    compact.insert(key.clone(), compact_log_value(item));
                }
            }
            Value::Object(compact)
        }
        Value::Array(items) => Value::Array(items.iter().take(24).map(compact_log_value).collect()),
        other => other.clone(),
    }
}

fn append_run_log(app: &AppHandle, run_id: &str, event: Value) {
    if run_id.is_empty() || run_id == "resolve" || run_id == "browser-live" || run_id == "planning"
    {
        return;
    }
    if !diagnostic_logging_enabled(app) {
        return;
    }
    let Ok(path) = run_log_path(app, run_id) else {
        return;
    };
    let mut record = match event {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("event".into(), other);
            map
        }
    };
    record
        .entry("at".to_string())
        .or_insert_with(|| Value::String(Utc::now().to_rfc3339()));
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{}", Value::Object(record));
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunLogSummary {
    run_id: String,
    path: String,
    updated_at: String,
    bytes: u64,
}

#[tauri::command]
fn list_run_logs(app: AppHandle) -> Result<Vec<RunLogSummary>, String> {
    let directory = run_logs_dir(&app)?;
    let mut logs = Vec::new();
    let entries = fs::read_dir(&directory).map_err(|error| error.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
            continue;
        }
        let metadata = entry.metadata().ok();
        logs.push(RunLogSummary {
            run_id: path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .into(),
            path: path.display().to_string(),
            updated_at: metadata
                .as_ref()
                .and_then(|info| info.modified().ok())
                .map(DateTime::<Utc>::from)
                .map(|time| time.to_rfc3339())
                .unwrap_or_default(),
            bytes: metadata.map(|info| info.len()).unwrap_or(0),
        });
    }
    logs.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    logs.truncate(40);
    Ok(logs)
}

#[tauri::command]
fn read_run_log(app: AppHandle, run_id: String) -> Result<String, String> {
    let path = run_log_path(&app, &run_id)?;
    let contents = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let line_count = contents.lines().count();
    if line_count <= 200 {
        return Ok(contents);
    }
    let skipped = line_count - 200;
    let tail = contents
        .lines()
        .skip(skipped)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("… {skipped} earlier events omitted …\n{tail}"))
}

#[tauri::command]
fn run_logs_folder(app: AppHandle) -> Result<String, String> {
    Ok(run_logs_dir(&app)?.display().to_string())
}

fn default_library(app: &AppHandle) -> String {
    app.path()
        .document_dir()
        .unwrap_or_else(|_| app_data_dir(app).unwrap_or_else(|_| PathBuf::from(".")))
        .join("Alfred Automations")
        .to_string_lossy()
        .to_string()
}

fn native_host_label() -> String {
    if cfg!(target_os = "windows") {
        "windows-ui-automation".into()
    } else if cfg!(target_os = "macos") {
        "macos-accessibility-pending".into()
    } else {
        "unsupported".into()
    }
}

#[tauri::command]
fn get_system_info(app: AppHandle) -> SystemInfo {
    SystemInfo {
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
        default_library_path: default_library(&app),
        native_host: native_host_label(),
    }
}

#[tauri::command]
fn get_settings(app: AppHandle) -> Result<AppSettings, String> {
    let path = settings_path(&app)?;
    if !path.exists() {
        let mut settings = AppSettings::default();
        settings.library_path = default_library(&app);
        return Ok(settings);
    }
    read_json_or_default(&path)
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: AppSettings) -> Result<AppSettings, String> {
    if settings.library_path.trim().is_empty() {
        return Err("Choose a workflow library folder before continuing.".into());
    }
    fs::create_dir_all(&settings.library_path).map_err(|error| error.to_string())?;
    write_json(&settings_path(&app)?, &settings)?;
    Ok(settings)
}

fn vault_entry(provider: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(VAULT_SERVICE, &format!("provider:{provider}"))
        .map_err(|error| format!("Credential vault is unavailable: {error}"))
}

fn vault_has(provider: &str) -> bool {
    vault_entry(provider)
        .and_then(|entry| entry.get_password().map_err(|e| e.to_string()))
        .is_ok()
}

#[tauri::command]
fn store_provider_secret(provider: String, secret: String) -> Result<(), String> {
    if secret.trim().is_empty() {
        return Err("The credential cannot be empty.".into());
    }
    vault_entry(&provider)?
        .set_password(secret.trim())
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn has_provider_secret(provider: String) -> bool {
    vault_has(&provider)
}

fn provider_definitions() -> [(&'static str, &'static str, &'static str); 4] {
    [
        ("codex", "OpenAI Codex", "codex"),
        ("copilot", "GitHub Copilot", "copilot"),
        ("cursor", "Cursor", "cursor-agent"),
        ("grok", "Grok", "grok"),
    ]
}

fn select_resolved_command(paths: Vec<PathBuf>, windows: bool) -> Option<PathBuf> {
    if !windows {
        return paths.into_iter().next();
    }
    paths
        .iter()
        .find(|path| {
            path.extension()
                .and_then(|value| value.to_str())
                .map(|value| value.eq_ignore_ascii_case("exe") || value.eq_ignore_ascii_case("com"))
                .unwrap_or(false)
        })
        .cloned()
        .or_else(|| {
            paths
                .into_iter()
                .find(|path| is_windows_command_script(path))
        })
}

fn resolve_provider_command(command: &str) -> Option<PathBuf> {
    let finder = if cfg!(target_os = "windows") {
        "where.exe"
    } else {
        "which"
    };
    let mut finder_command = Command::new(finder);
    hide_windows_console(&mut finder_command);
    let output = finder_command.arg(command).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let paths = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect();
    select_resolved_command(paths, cfg!(target_os = "windows"))
}

fn is_windows_command_script(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| value.eq_ignore_ascii_case("cmd") || value.eq_ignore_ascii_case("bat"))
        .unwrap_or(false)
}

fn quote_windows_command_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[derive(Debug, PartialEq)]
struct ResolvedProcess {
    program: PathBuf,
    args: Vec<String>,
    windows_raw_argument: Option<String>,
}

fn windows_command_script_process(
    path: &Path,
    args: &[String],
    allow_script_wrapper: bool,
) -> Result<ResolvedProcess, String> {
    if !allow_script_wrapper {
        return Err(format!(
            "{} is installed as a command script that Alfred cannot safely supervise. Install the provider's native Windows executable.",
            path.display()
        ));
    }
    let mut command_line = quote_windows_command_argument(&path.to_string_lossy());
    for argument in args {
        command_line.push(' ');
        command_line.push_str(&quote_windows_command_argument(argument));
    }
    let shell = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("cmd.exe"));
    Ok(ResolvedProcess {
        program: shell,
        args: vec!["/D".into(), "/S".into(), "/C".into()],
        // cmd.exe does not use the C runtime's argument escaping rules. Appending
        // this command text as a raw argument avoids turning its quotes into \".
        windows_raw_argument: Some(format!("\"{command_line}\"")),
    })
}

fn resolved_process(
    path: &Path,
    args: &[String],
    allow_script_wrapper: bool,
) -> Result<ResolvedProcess, String> {
    if cfg!(target_os = "windows") && is_windows_command_script(path) {
        return windows_command_script_process(path, args, allow_script_wrapper);
    }
    Ok(ResolvedProcess {
        program: path.to_path_buf(),
        args: args.to_vec(),
        windows_raw_argument: None,
    })
}

fn provider_version(path: &Path) -> Option<String> {
    let args = vec!["--version".to_string()];
    let resolved = resolved_process(path, &args, true).ok()?;
    let mut process = Command::new(resolved.program);
    hide_windows_console(&mut process);
    process.args(resolved.args);
    #[cfg(target_os = "windows")]
    if let Some(raw_argument) = resolved.windows_raw_argument {
        process.raw_arg(raw_argument);
    }
    let output = process.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    (!stdout.is_empty())
        .then_some(stdout)
        .or_else(|| (!stderr.is_empty()).then_some(stderr))
}

#[tauri::command]
fn detect_providers() -> Vec<ProviderStatus> {
    provider_definitions()
        .into_iter()
        .map(|(id, name, command)| {
            let resolved = resolve_provider_command(command);
            let installed = resolved.is_some();
            let version = resolved.as_deref().and_then(provider_version);
            ProviderStatus {
                id: id.into(),
                name: name.into(),
                command: command.into(),
                installed,
                version,
                credential_stored: vault_has(id),
            }
        })
        .collect()
}

/// How a provider CLI receives images. The models behind every supported CLI are
/// multimodal; only the delivery pipe differs:
/// - Flag: Codex `-i/--image <FILE>...`, Copilot `--attachment <path>` (valid in
///   the non-interactive -p mode Alfred uses).
/// - PromptPaths: Grok and Cursor have no image flag, but their built-in
///   file-reading tools hand image files to the multimodal model (verified live
///   against the Grok CLI: a single read_file call returned full visual
///   understanding). For these, Alfred lists the screenshot paths in the prompt.
enum ImageDelivery {
    Flag,
    PromptPaths,
}

fn provider_image_delivery(provider: &str) -> Option<ImageDelivery> {
    match provider {
        "codex" | "copilot" => Some(ImageDelivery::Flag),
        "grok" | "cursor" => Some(ImageDelivery::PromptPaths),
        _ => None,
    }
}

fn provider_invocation(
    provider: &str,
    prompt: &str,
    images: &[PathBuf],
) -> Result<ProviderInvocation, String> {
    provider_invocation_for_session(provider, prompt, images, None, false)
}

/// Builds either the first turn of a provider conversation or an explicit
/// continuation. Alfred still sends the complete durable memory each turn, so a
/// provider losing its own session can never erase the run's state.
fn provider_invocation_for_session(
    provider: &str,
    prompt: &str,
    images: &[PathBuf],
    session_id: Option<&str>,
    resume: bool,
) -> Result<ProviderInvocation, String> {
    let (args, stdin) =
        match provider {
            "codex" => {
                let mut args = vec![
                    "--ask-for-approval",
                    "never",
                    "--sandbox",
                    "read-only",
                    "exec",
                ];
                if resume {
                    args.push("resume");
                }
                args.extend(["--json", "--ignore-user-config", "--skip-git-repo-check"]);
                if resume {
                    args.push(session_id.ok_or_else(|| {
                        "Codex continuation is missing its session id.".to_string()
                    })?);
                }
                args.push("-");
                (args, Some(prompt.to_string()))
            }
            "copilot" => {
                let mut args = vec![
                    "-p",
                    prompt,
                    "-s",
                    "--output-format",
                    "json",
                    "--no-ask-user",
                    "--no-custom-instructions",
                    "--disable-builtin-mcps",
                    "--deny-tool=shell,write,url,memory",
                ];
                if let Some(id) = session_id {
                    args.extend(["--session-id", id]);
                }
                (args, None)
            }
            "cursor" => {
                let mut args = vec!["-p", "--output-format", "stream-json"];
                let resume_arg;
                if resume {
                    resume_arg = format!(
                        "--resume={}",
                        session_id.ok_or_else(|| {
                            "Cursor continuation is missing its session id.".to_string()
                        })?
                    );
                    args.push(&resume_arg);
                }
                args.push(prompt);
                let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
                return finish_provider_invocation(provider, args, None, images);
            }
            // Prefer whole-message JSON: streaming-json emits token-level
            // {"type":"text","data":"…"} fragments that must be reassembled. The
            // `json` format returns one object with a complete `text` field.
            "grok" => {
                let mut args = vec![
                    "-p",
                    prompt,
                    "--output-format",
                    "json",
                    "--tools",
                    "read_file",
                    "--no-subagents",
                ];
                if let Some(id) = session_id {
                    args.extend(if resume {
                        vec!["--resume", id]
                    } else {
                        vec!["--session-id", id]
                    });
                }
                (args, None)
            }
            _ => return Err(format!("Unknown provider: {provider}")),
        };
    let args: Vec<String> = args.into_iter().map(str::to_string).collect();
    finish_provider_invocation(provider, args, stdin, images)
}

fn finish_provider_invocation(
    provider: &str,
    mut args: Vec<String>,
    stdin: Option<String>,
    images: &[PathBuf],
) -> Result<ProviderInvocation, String> {
    if !images.is_empty() {
        match provider {
            "codex" => {
                // codex takes `-i/--image <FILE>...` (comma-separated); it must
                // precede the positional prompt argument.
                let joined = images
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let prompt_arg = args.pop();
                args.push("--image".into());
                args.push(joined);
                if let Some(prompt_arg) = prompt_arg {
                    args.push(prompt_arg);
                }
            }
            // copilot takes one --attachment <path> per image; in -p mode flags
            // may trail the prompt.
            "copilot" => {
                for image in images {
                    args.push("--attachment".into());
                    args.push(image.to_string_lossy().to_string());
                }
            }
            _ => {}
        }
    }
    let command = provider_definitions()
        .into_iter()
        .find(|item| item.0 == provider)
        .map(|item| item.2)
        .ok_or_else(|| format!("Unknown provider: {provider}"))?;
    Ok(ProviderInvocation {
        command: command.into(),
        args,
        stdin,
    })
}

/// Builds a supervised provider CLI invocation: resolved executable path (with the
/// Windows command-script wrapper where a provider installs as .cmd), sandboxed
/// arguments, stdin payload when the CLI reads prompts from stdin, and the
/// OS-vault credential injected into the environment. Shared by design-time
/// planning sessions and runtime agent-loop turns.
fn provider_command(
    app: &AppHandle,
    provider: &str,
    prompt: &str,
    images: &[PathBuf],
    session_id: Option<&str>,
    resume: bool,
) -> Result<(tokio::process::Command, Option<String>), String> {
    let invocation = provider_invocation_for_session(provider, prompt, images, session_id, resume)?;
    let resolved = resolve_provider_command(&invocation.command).ok_or_else(|| {
        format!(
            "{} is not available to Alfred. Install it, sign in, then restart Alfred.",
            invocation.command
        )
    })?;
    let resolved = resolved_process(&resolved, &invocation.args, provider == "codex")?;
    let planner_workspace = app_data_dir(app)?.join("planner-workspace");
    fs::create_dir_all(&planner_workspace).map_err(|error| error.to_string())?;
    let mut process = tokio::process::Command::new(&resolved.program);
    #[cfg(windows)]
    {
        process.creation_flags(CREATE_NO_WINDOW);
    }
    process
        .args(resolved.args)
        .current_dir(planner_workspace)
        .stdin(if invocation.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    if let Some(raw_argument) = resolved.windows_raw_argument {
        process.as_std_mut().raw_arg(raw_argument);
    }
    if let Ok(secret) = vault_entry(provider)
        .and_then(|entry| entry.get_password().map_err(|error| error.to_string()))
    {
        match provider {
            "codex" => {
                process.env("OPENAI_API_KEY", secret);
            }
            "copilot" => {
                process.env("GH_TOKEN", secret);
            }
            "cursor" => {
                process.env("CURSOR_API_KEY", secret);
            }
            "grok" => {
                process.env("XAI_API_KEY", secret);
            }
            _ => {}
        }
    }
    Ok((process, invocation.stdin))
}

/// Preflight used when a goal run starts: resolves the configured planner CLI
/// exactly the way provider_command does — including the Windows command-script
/// policy — without spawning anything. A missing or unsupervisable CLI fails the
/// invoke immediately, so the cockpit shows a clear error instead of starting a
/// run whose planner turns all die before the first timeline event arrives.
fn preflight_provider(provider: &str) -> Result<(), String> {
    let invocation = provider_invocation(provider, "preflight", &[])?;
    let resolved = resolve_provider_command(&invocation.command).ok_or_else(|| {
        format!(
            "{} is not available to Alfred. Install it, sign in, then restart Alfred.",
            invocation.command
        )
    })?;
    let _ = resolved_process(&resolved, &invocation.args, provider == "codex")?;
    Ok(())
}

const ALLOWED_PLAN_METHODS: &[&str] = &[
    "launchApplication",
    "listApplications",
    "listInstalledApplications",
    "activate",
    "focusApplication",
    "navigateApplication",
    "observeWindow",
    "captureWindow",
    "findElement",
    "getValue",
    "invokeElement",
    "setValue",
    "click",
    "typeText",
    "key",
    "shortcut",
    "probe",
    "scroll",
    "rightClick",
    "doubleClick",
    "hover",
    "drag",
    "browser.observe",
    "browser.navigate",
    "browser.click",
    "browser.type",
    "browser.getText",
    "browser.read",
    "browser.scroll",
    "browser.find",
    "browser.wait",
    "browser.hover",
    "browser.dblclick",
];

const COMMIT_VERBS: &[&str] = &[
    "post", "send", "publish", "share", "submit", "tweet", "save", "save as", "install",
];

fn kind_is_always_external_write(method: &str) -> bool {
    matches!(
        method,
        "launchApplication" | "navigateApplication" | "browser.navigate"
    )
}

fn is_commit_label(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    let tokens: Vec<&str> = normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    COMMIT_VERBS.iter().any(|verb| {
        if *verb == "save as" {
            normalized.contains("save as")
        } else {
            tokens.iter().any(|token| token == verb)
        }
    })
}

fn is_commit_action(kind: &str, target: Option<&str>, payload: Option<&Value>) -> bool {
    if kind == "shortcut" {
        return payload
            .and_then(|value| value.get("keys"))
            .and_then(Value::as_str)
            .is_some_and(|keys| keys.eq_ignore_ascii_case("CTRL+S"));
    }
    if !matches!(
        kind,
        "invokeElement" | "click" | "doubleClick" | "browser.click" | "browser.dblclick"
    ) {
        return false;
    }
    if target.is_some_and(is_commit_label) {
        return true;
    }
    payload.is_some_and(|value| {
        ["name", "text", "mark"]
            .into_iter()
            .any(|key| value.get(key).and_then(Value::as_str).is_some_and(is_commit_label))
    })
}

fn method_effect(method: &str) -> &'static str {
    // Default floor from the method alone. Commit verbs on invoke/click are
    // applied in method_effect_for once the target/payload is known.
    if kind_is_observe(method) {
        "observe"
    } else if kind_is_always_external_write(method) {
        "external_write"
    } else {
        "modify_reversible"
    }
}

fn method_effect_for(method: &str, target: Option<&str>, payload: Option<&Value>) -> &'static str {
    if kind_is_observe(method) {
        "observe"
    } else if kind_is_always_external_write(method) || is_commit_action(method, target, payload) {
        "external_write"
    } else {
        "modify_reversible"
    }
}

fn is_native_browser_application(application: &str) -> bool {
    matches!(
        application.to_ascii_lowercase().as_str(),
        "microsoft edge" | "google chrome" | "brave"
    )
}

fn is_safe_http_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 || trimmed.chars().any(char::is_control) {
        return false;
    }
    let authority = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .and_then(|rest| rest.split(['/', '?', '#']).next());
    if authority.is_none_or(|value| value.is_empty() || value.contains('@')) {
        return false;
    }
    tauri::Url::parse(trimmed).is_ok_and(|parsed| {
        matches!(parsed.scheme(), "https" | "http")
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
    })
}

fn json_finite_unit_interval(value: &Value, key: &str) -> Option<f64> {
    let number = value.get(key)?;
    let parsed = number
        .as_f64()
        .or_else(|| number.as_i64().map(|value| value as f64))
        .or_else(|| number.as_u64().map(|value| value as f64))
        .or_else(|| number.as_str()?.parse().ok())?;
    if parsed.is_finite() && (0.0..=1.0).contains(&parsed) {
        Some(parsed)
    } else {
        None
    }
}

fn payload_has_normalized_point(payload: Option<&Value>) -> bool {
    let Some(value) = payload else {
        return false;
    };
    json_finite_unit_interval(value, "nx").is_some() && json_finite_unit_interval(value, "ny").is_some()
}

fn native_browser_application(applications: &[String]) -> String {
    applications
        .iter()
        .find(|application| is_native_browser_application(application))
        .cloned()
        .unwrap_or_else(|| "Microsoft Edge".into())
}

fn validate_workflow_step(step: &WorkflowStep) -> Result<(), String> {
    if !ALLOWED_PLAN_METHODS.contains(&step.kind.as_str()) {
        return Err(format!("Unsupported workflow method: {}", step.kind));
    }
    let _application = step
        .application
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Every workflow step must name an application.".to_string())?;
    let empty_params = serde_json::json!({});
    let params = step.payload.as_ref().unwrap_or(&empty_params);
    if !params.is_object() {
        return Err(format!(
            "Parameters for {} must be a JSON object.",
            step.kind
        ));
    }
    if step.kind == "typeText"
        && params
            .get("text")
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true)
    {
        return Err("A typeText step must include non-empty text.".into());
    }
    if step.kind == "key" && params.get("virtualKey").and_then(Value::as_u64) == Some(0x2e) {
        return Err("The Delete key is blocked by Alfred's deletion policy.".into());
    }
    if step.kind == "shortcut"
        && !params
            .get("keys")
            .and_then(Value::as_str)
            .map(|keys| matches!(keys.to_ascii_uppercase().as_str(), "CTRL+L" | "CTRL+S"))
            .unwrap_or(false)
    {
        return Err("Only CTRL+L and CTRL+S are allowed shortcuts.".into());
    }
    if step.kind == "navigateApplication" {
        let application = step.application.as_deref().unwrap_or_default();
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_native_browser_application(application) || !is_safe_http_url(url) {
            return Err(
                "navigateApplication requires Edge, Chrome, or Brave and an absolute HTTP(S) URL."
                    .into(),
            );
        }
    }
    if step.kind == "browser.navigate" {
        let url = params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_safe_http_url(url) {
            return Err("browser.navigate requires an absolute HTTP(S) URL.".into());
        }
    }
    if matches!(
        step.kind.as_str(),
        "invokeElement"
            | "click"
            | "rightClick"
            | "doubleClick"
            | "hover"
            | "browser.click"
            | "browser.type"
            | "browser.hover"
            | "browser.dblclick"
    ) && step
        .target_label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(format!("{} requires a visible target label.", step.kind));
    }
    if step.kind == "probe"
        && (params.get("nx").and_then(Value::as_f64).is_none()
            || params.get("ny").and_then(Value::as_f64).is_none())
    {
        return Err("probe requires nx and ny in window bitmap space (0–1).".into());
    }
    if step.kind == "drag"
        && (params
            .get("from")
            .and_then(Value::as_str)
            .map(str::is_empty)
            .unwrap_or(true)
            || params
                .get("to")
                .and_then(Value::as_str)
                .map(str::is_empty)
                .unwrap_or(true))
    {
        return Err("drag requires from and to marks.".into());
    }
    if step.kind == "click" {
        let has_mark = params
            .get("mark")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        let has_normalized = payload_has_normalized_point(Some(params));
        let has_pixels = params.get("x").is_some() && params.get("y").is_some();
        if !has_mark && !has_normalized && !has_pixels {
            return Err("click requires a mark, nx/ny, or recorded x/y.".into());
        }
    }
    let expected = method_effect_for(
        &step.kind,
        step.target_label.as_deref(),
        step.payload.as_ref(),
    );
    if step.effect != expected {
        return Err(format!(
            "The effect for {} does not match its method.",
            step.kind
        ));
    }
    Ok(())
}

fn strip_json_fence(value: &str) -> &str {
    let trimmed = value.trim();
    let without_start = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    without_start
        .strip_suffix("```")
        .unwrap_or(without_start)
        .trim()
}

#[cfg(test)]
fn plan_from_json_value(value: Value) -> Option<ProviderPlan> {
    if value.is_array() {
        return serde_json::from_value::<Vec<ProviderPlanStep>>(value)
            .ok()
            .map(|steps| ProviderPlan { steps });
    }
    serde_json::from_value(value).ok()
}

#[cfg(test)]
fn plan_from_text(value: &str) -> Option<ProviderPlan> {
    let candidate = strip_json_fence(value);
    if let Ok(parsed) = serde_json::from_str::<Value>(candidate) {
        if let Some(plan) = plan_from_json_value(parsed) {
            return Some(plan);
        }
    }
    let object_start = candidate.find('{');
    let object_end = candidate.rfind('}');
    if let (Some(start), Some(end)) = (object_start, object_end) {
        if start < end {
            if let Ok(parsed) = serde_json::from_str::<Value>(&candidate[start..=end]) {
                if let Some(plan) = plan_from_json_value(parsed) {
                    return Some(plan);
                }
            }
        }
    }
    let array_start = candidate.find('[');
    let array_end = candidate.rfind(']');
    if let (Some(start), Some(end)) = (array_start, array_end) {
        if start < end {
            if let Ok(parsed) = serde_json::from_str::<Value>(&candidate[start..=end]) {
                return plan_from_json_value(parsed);
            }
        }
    }
    None
}

fn collect_provider_text(value: &Value, output: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if matches!(
                    key.as_str(),
                    "text"
                        | "output_text"
                        | "content"
                        | "result"
                        | "message"
                        | "delta"
                        | "completion"
                        | "response"
                        | "answer"
                        | "output"
                        | "partial_response"
                ) {
                    if let Some(text) = item.as_str() {
                        output.push(text.to_string());
                    }
                }
                collect_provider_text(item, output);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_provider_text(item, output);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
fn validate_provider_plan(plan: ProviderPlan) -> Result<Vec<WorkflowStep>, String> {
    if plan.steps.is_empty() {
        return Err("The provider returned an empty plan.".into());
    }
    if plan.steps.len() > 100 {
        return Err("The provider returned too many workflow steps.".into());
    }
    plan.steps
        .into_iter()
        .map(|item| {
            let method = item.method.trim().to_string();
            let application = item.application.trim().to_string();
            if !ALLOWED_PLAN_METHODS.contains(&method.as_str()) {
                return Err(format!(
                    "The provider proposed an unsupported method: {method}"
                ));
            }
            if application.is_empty() {
                return Err("Every provider step must name an application.".into());
            }
            if !item.params.is_object() {
                return Err(format!("Parameters for {method} must be a JSON object."));
            }
            if method == "typeText"
                && item
                    .params
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::is_empty)
                    .unwrap_or(true)
            {
                return Err("A typeText step must include non-empty text.".into());
            }
            let effect = method_effect_for(&method, item.target_label.as_deref(), Some(&item.params))
                .to_string();
            let title = if item.title.trim().is_empty() {
                item.target_label.clone().unwrap_or_else(|| method.clone())
            } else {
                item.title.trim().to_string()
            };
            let step = WorkflowStep {
                id: Uuid::new_v4().to_string(),
                title,
                kind: method.clone(),
                effect: effect.clone(),
                application: Some(application.clone()),
                intent: Some(
                    format!("{method} {}", item.target_label.clone().unwrap_or_default())
                        .trim()
                        .to_string(),
                ),
                target_label: item.target_label,
                payload: Some(item.params),
                timeout_ms: default_timeout(),
                retries: default_retries(),
                wait_for: None,
                expect: None,
                save_as: None,
            };
            validate_workflow_step(&step)?;
            let decision = evaluate_base_policy(&ActionRequest {
                protocol_version: protocol_version(),
                run_id: "planning".into(),
                workflow_step: step.id.clone(),
                application,
                intent: step.intent.clone().unwrap_or_default(),
                effect,
                target_label: step.target_label.clone(),
                payload: step.payload.clone(),
            });
            if decision.decision != "allow" {
                return Err(decision.reason);
            }
            Ok(step)
        })
        .collect()
}

#[cfg(test)]
fn parse_provider_plan_output(output: &[String]) -> Result<Vec<WorkflowStep>, String> {
    let mut candidates = Vec::new();
    for line in output.iter().rev() {
        if let Ok(value) = serde_json::from_str::<Value>(line) {
            if let Some(plan) = plan_from_json_value(value.clone()) {
                return validate_provider_plan(plan);
            }
            collect_provider_text(&value, &mut candidates);
        }
        candidates.push(line.clone());
    }
    for candidate in candidates.iter().rev() {
        if let Some(plan) = plan_from_text(candidate) {
            return validate_provider_plan(plan);
        }
    }
    Err("Alfred could not find a valid JSON workflow plan in the provider output.".into())
}

fn workflow_path(library_path: &str, workflow: &Workflow) -> PathBuf {
    let safe_name: String = workflow
        .name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == ' ' || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect();
    Path::new(library_path)
        .join(format!("{}-{}", safe_name.trim(), &workflow.id[..8]))
        .join("workflow.yaml")
}

fn locate_workflow(library_path: &str, workflow_id: &str) -> Result<PathBuf, String> {
    for entry in fs::read_dir(library_path).map_err(|error| error.to_string())? {
        let path = entry
            .map_err(|error| error.to_string())?
            .path()
            .join("workflow.yaml");
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(workflow) = serde_yaml::from_str::<Workflow>(&contents) {
                if workflow.id == workflow_id {
                    return Ok(path);
                }
            }
        }
    }
    Err("Workflow was not found in the selected library.".into())
}

fn load_workflow(library_path: &str, workflow_id: &str) -> Result<(PathBuf, Workflow), String> {
    let path = locate_workflow(library_path, workflow_id)?;
    let contents = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let workflow = serde_yaml::from_str(&contents).map_err(|error| error.to_string())?;
    Ok((path, workflow))
}

fn save_workflow(path: &Path, workflow: &Workflow) -> Result<(), String> {
    let contents = serde_yaml::to_string(workflow).map_err(|error| error.to_string())?;
    atomic_write(path, contents.as_bytes())
}

#[tauri::command]
fn list_workflows(library_path: String) -> Result<Vec<Workflow>, String> {
    let root = Path::new(&library_path);
    if !root.exists() {
        fs::create_dir_all(root).map_err(|error| error.to_string())?;
        return Ok(Vec::new());
    }
    let mut workflows = Vec::new();
    for entry in fs::read_dir(root).map_err(|error| error.to_string())? {
        let path = entry
            .map_err(|error| error.to_string())?
            .path()
            .join("workflow.yaml");
        if let Ok(contents) = fs::read_to_string(&path) {
            if let Ok(workflow) = serde_yaml::from_str::<Workflow>(&contents) {
                workflows.push(workflow);
            }
        }
    }
    workflows.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(workflows)
}

fn is_persistent_data_loss_phrase(value: &str) -> bool {
    const PHRASES: &[&str] = &[
        "empty recycle",
        "empty trash",
        "empty bin",
        "to trash",
        "to the trash",
        "to recycle",
        "recycle bin",
        "permanently delete",
        "delete permanently",
        "uninstall",
        "format drive",
        "drop table",
        "wipe disk",
        "shred",
        "purge records",
        "purge all",
        "destroy account",
        "overwrite existing",
        "replace file",
        "revoke access",
        "clear history",
        "shift+delete",
    ];
    let lowered = value.to_ascii_lowercase();
    PHRASES.iter().any(|phrase| lowered.contains(phrase))
}

fn policy_tokens(value: &str) -> Vec<String> {
    value
        .to_ascii_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn is_destruction_verb(token: &str) -> bool {
    matches!(
        token,
        "delete"
            | "remove"
            | "erase"
            | "destroy"
            | "purge"
            | "uninstall"
            | "trash"
            | "overwrite"
            | "wipe"
    )
}

/// A delete/remove/erase is reversible only when its *object* is a draft, filter,
/// selection, highlight, or formatting — and no durable noun sits in that same
/// local window. Mixed clauses ("remove the filter then delete the account")
/// stay destructive because each verb is classified independently.
fn verb_is_reversible(tokens: &[String], index: usize) -> bool {
    const HINTS: &[&str] = &["filter", "draft", "selection", "highlight", "formatting"];
    const BLOCK: &[&str] = &[
        "account",
        "user",
        "file",
        "email",
        "message",
        "item",
        "post",
        "mail",
        "member",
        "project",
        "task",
        "workspace",
        "folder",
        "permanently",
        "data",
    ];
    if !matches!(tokens[index].as_str(), "delete" | "remove" | "erase") {
        return false;
    }
    // A durable noun anywhere in the label keeps the action destructive, even
    // when it sits outside the local verb window ("remove the filter and the user").
    if tokens.iter().any(|item| BLOCK.contains(&item.as_str())) {
        return false;
    }
    let window = &tokens[index..index.saturating_add(4).min(tokens.len())];
    let hits_hint = window.iter().any(|item| HINTS.contains(&item.as_str()));
    if !hits_hint {
        return false;
    }
    // "Delete draft" often destroys a persisted mail/doc draft. Only treat draft
    // as reversible when the object is clearly in-progress text/selection.
    if window.iter().any(|item| item.as_str() == "draft")
        && !window
            .iter()
            .any(|item| matches!(item.as_str(), "text" | "selection" | "highlight" | "formatting"))
    {
        return false;
    }
    true
}

#[cfg(test)]
fn is_reversible_remove(value: &str) -> bool {
    let tokens = policy_tokens(value);
    let mut saw_reversible = false;
    let mut saw_irreversible = false;
    for (index, token) in tokens.iter().enumerate() {
        if !is_destruction_verb(token) {
            continue;
        }
        if verb_is_reversible(&tokens, index) {
            saw_reversible = true;
        } else {
            saw_irreversible = true;
        }
    }
    saw_reversible && !saw_irreversible
}

fn is_destruction_target(target: &str) -> bool {
    let trimmed = target.trim().to_ascii_lowercase();
    matches!(
        trimmed.as_str(),
        "delete"
            | "delete item"
            | "delete file"
            | "delete email"
            | "delete message"
            | "delete account"
            | "delete user"
            | "delete post"
            | "remove user"
            | "remove account"
            | "remove member"
            | "empty recycle bin"
            | "empty trash"
            | "move to recycle bin"
    ) || {
        let tokens = policy_tokens(&trimmed);
        tokens
            .iter()
            .enumerate()
            .any(|(index, token)| is_destruction_verb(token) && !verb_is_reversible(&tokens, index))
    }
}

fn payload_has_persistent_data_loss(payload: Option<&Value>) -> bool {
    let Some(Value::Object(map)) = payload else {
        return false;
    };
    map.iter().any(|(key, value)| {
        if matches!(
            key.as_str(),
            "text" | "value" | "url" | "mark" | "from" | "to" | "generation" | "processId" | "nx" | "ny" | "x" | "y"
        ) {
            return false;
        }
        value.as_str().is_some_and(|text| {
            is_destruction_target(text) || is_persistent_data_loss_phrase(text)
        })
    })
}

pub fn evaluate_base_policy(request: &ActionRequest) -> ActionDecision {
    let intent = request.intent.to_lowercase();
    let effect = request.effect.to_lowercase();
    let target = request
        .target_label
        .clone()
        .unwrap_or_default()
        .to_lowercase();
    let destructive = effect == "destructive"
        || is_destruction_target(&target)
        || is_destruction_target(&intent)
        || is_persistent_data_loss_phrase(&intent)
        || is_persistent_data_loss_phrase(&target)
        || payload_has_persistent_data_loss(request.payload.as_ref());
    if destructive {
        return ActionDecision {
            decision: "hard_deny".into(),
            reason: format!(
                "Alfred blocked an irreversible data-loss action in {}.",
                request.application
            ),
            rule: "persistent-data-loss".into(),
        };
    }
    // Raw virtual-key codes are a non-semantic channel: keyword filters cannot see
    // what 0x2E does, so the policy must. The Delete key can destroy data in the
    // focused application and is denied outright; the native host independently
    // enforces an allow-list of safe keys as defense in depth.
    let virtual_key = request
        .payload
        .as_ref()
        .and_then(|payload| payload.get("virtualKey"))
        .and_then(Value::as_i64);
    if virtual_key == Some(0x2E) {
        return ActionDecision {
            decision: "hard_deny".into(),
            reason: "Alfred blocked the Delete key. Deletion keystrokes are never automated."
                .into(),
            rule: "persistent-data-loss".into(),
        };
    }
    if effect == "unknown" {
        return ActionDecision {
            decision: "request_user".into(),
            reason: "The effect of this action is unclear and needs confirmation.".into(),
            rule: "unknown-effects-require-review".into(),
        };
    }
    ActionDecision {
        decision: "allow".into(),
        reason: "The action passed Alfred's non-destructive safety policy.".into(),
        rule: "non-destructive-action".into(),
    }
}

fn read_permissions(app: &AppHandle) -> Result<Vec<PermissionGrant>, String> {
    read_json_or_default(&permissions_path(app)?)
}

/// Observation methods never change state; everything else does. A declared
/// effect is untrusted input — a planner can be prompt-injected, and a YAML file
/// can be hand-edited — so the floor comes from the method: mutating methods can
/// never run as "observe", which would skip the permission grant entirely.
fn kind_is_observe(kind: &str) -> bool {
    matches!(
        kind.to_ascii_lowercase().as_str(),
        "browser.observe"
            | "browser.capturevisible"
            | "observewindow"
            | "capturewindow"
            | "findelement"
            | "getvalue"
            | "probe"
            | "scroll"
            | "browser.gettext"
            | "browser.read"
            | "browser.scroll"
            | "browser.find"
            | "browser.wait"
            | "listapplications"
            | "listinstalledapplications"
            | "resolveapplication"
            | "health"
    )
}

fn effective_effect(kind: &str, declared: &str) -> String {
    effective_effect_for(kind, declared, None, None)
}

fn effective_effect_for(
    kind: &str,
    declared: &str,
    target: Option<&str>,
    payload: Option<&Value>,
) -> String {
    // Supported methods have an Alfred-owned classification. A planner may omit
    // or mislabel `effect`, but that must neither bypass the safety floor nor
    // create an approval prompt for a method Alfred already understands.
    if ALLOWED_PLAN_METHODS.contains(&kind) {
        return method_effect_for(kind, target, payload).into();
    }
    if declared == "observe" && !kind_is_observe(kind) {
        "unknown".into()
    } else {
        declared.to_string()
    }
}

#[tauri::command]
fn list_permissions(app: AppHandle) -> Result<Vec<PermissionGrant>, String> {
    read_permissions(&app)
}

#[tauri::command]
fn grant_permission(
    app: AppHandle,
    application: String,
    allowed_effects: Vec<String>,
    allowed_intents: Vec<String>,
) -> Result<PermissionGrant, String> {
    if allowed_effects.iter().any(|effect| effect == "destructive") {
        return Err("Destructive permission cannot be granted.".into());
    }
    let mut permissions = read_permissions(&app)?;
    let grant = PermissionGrant {
        id: Uuid::new_v4().to_string(),
        application,
        allowed_effects,
        allowed_intents,
        enabled: true,
        created_at: Utc::now(),
    };
    permissions.push(grant.clone());
    write_json(&permissions_path(&app)?, &permissions)?;
    Ok(grant)
}

#[tauri::command]
fn set_permission_enabled(
    app: AppHandle,
    permission_id: String,
    enabled: bool,
) -> Result<Vec<PermissionGrant>, String> {
    let mut permissions = read_permissions(&app)?;
    let permission = permissions
        .iter_mut()
        .find(|item| item.id == permission_id)
        .ok_or_else(|| "Permission not found.".to_string())?;
    permission.enabled = enabled;
    write_json(&permissions_path(&app)?, &permissions)?;
    Ok(permissions)
}

#[tauri::command]
fn evaluate_action(app: AppHandle, request: ActionRequest) -> Result<ActionDecision, String> {
    let base = evaluate_base_policy(&request);
    if base.decision != "allow" {
        return Ok(base);
    }
    // The safety engine, rather than a pre-existing per-application grant, is
    // authoritative for ordinary actions. Once the base policy has classified
    // an action as non-destructive it may run immediately. Unknown effects still
    // request review above, and destructive actions remain an absolute denial.
    let _ = app;
    Ok(ActionDecision {
        decision: "allow".into(),
        reason: "The safety engine classified this action as non-destructive.".into(),
        rule: "safe-action-auto-approved".into(),
    })
}

fn browser_token_path(app: &AppHandle) -> Result<PathBuf, String> {
    #[cfg(windows)]
    if let Ok(root) = std::env::var("LOCALAPPDATA") {
        return Ok(PathBuf::from(root).join("Alfred/browser-bridge-token"));
    }
    Ok(app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("browser-bridge-token"))
}

fn connect_browser_bridge() -> Result<TcpStream, String> {
    let address: SocketAddr = "127.0.0.1:17844"
        .parse()
        .map_err(|error: std::net::AddrParseError| error.to_string())?;
    let stream = TcpStream::connect_timeout(&address, std::time::Duration::from_secs(2))
        .map_err(|_| "The Alfred browser extension is not connected. Open the installed browser and enable the Alfred extension.".to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    Ok(stream)
}

#[tauri::command]
fn browser_bridge_status(app: AppHandle) -> Result<bool, String> {
    Ok(browser_token_path(&app)?.exists() && connect_browser_bridge().is_ok())
}

fn send_browser_command_inner(
    app: AppHandle,
    command: BrowserCommand,
    approval_override: bool,
) -> Result<Value, String> {
    let decision = evaluate_action(
        app.clone(),
        ActionRequest {
            protocol_version: protocol_version(),
            run_id: "browser-live".into(),
            workflow_step: command.id.clone(),
            application: "Installed browser".into(),
            intent: command.intent.clone(),
            effect: command.effect.clone(),
            target_label: command.target_label.clone(),
            payload: Some(command.params.clone()),
        },
    )?;
    let overridden = approval_override && decision.decision == "request_user";
    if decision.decision != "allow" && !overridden {
        return Err(format!("{}: {}", decision.decision, decision.reason));
    }
    if command.method == "navigate" {
        let url = command
            .params
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !is_safe_http_url(url) {
            return Err("browser.navigate requires an absolute HTTP(S) URL.".into());
        }
    }
    let token = fs::read_to_string(browser_token_path(&app)?)
        .map_err(|_| "The browser bridge has not been paired yet.".to_string())?;
    let method_name = command.method.clone();
    let mut request = command.params.as_object().cloned().unwrap_or_default();
    request.insert("id".into(), Value::String(command.id));
    request.insert("method".into(), Value::String(method_name.clone()));
    request.insert("effect".into(), Value::String(command.effect));
    request.insert("intent".into(), Value::String(command.intent));
    if let Some(target) = command.target_label {
        request.insert("targetLabel".into(), Value::String(target));
    }
    let envelope = serde_json::json!({ "capabilityToken": token.trim(), "request": request });
    let mut stream = connect_browser_bridge()?;
    writeln!(stream, "{}", envelope).map_err(|error| error.to_string())?;
    let mut line = String::new();
    StdBufReader::new(stream)
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    // The bridge reports failures inside the envelope; surface them so runs fail
    // (and retry) honestly instead of treating a rejected command as success.
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Browser action failed.")
            .to_string();
        if let Some(run_id) = command.run_id.as_deref() {
            append_run_log(
                &app,
                run_id,
                serde_json::json!({
                    "kind": "tool",
                    "channel": "browser",
                    "method": method_name,
                    "ok": false,
                    "error": error,
                    "payload": compact_log_value(&command.params)
                }),
            );
        }
        return Err(error);
    }
    if let Some(run_id) = command.run_id.as_deref() {
        append_run_log(
            &app,
            run_id,
            serde_json::json!({
                "kind": "tool",
                "channel": "browser",
                "method": method_name,
                "ok": true,
                "result": compact_log_value(&value)
            }),
        );
    }
    Ok(value)
}

#[tauri::command]
fn send_browser_command(app: AppHandle, command: BrowserCommand) -> Result<Value, String> {
    send_browser_command_inner(app, command, false)
}

#[cfg(test)]
#[allow(dead_code)]
fn substitute_text(text: &str, variables: &HashMap<String, String>) -> String {
    let mut result = text.to_string();
    for (key, replacement) in variables {
        result = result.replace(&format!("${{{key}}}"), replacement);
    }
    result
}

/// Replaces `${name}` placeholders in every string of a step payload with values
/// captured by earlier steps, so data can flow from one application into another.
#[cfg(test)]
#[allow(dead_code)]
fn substitute_variables(value: &mut Value, variables: &HashMap<String, String>) {
    match value {
        Value::String(text) => *text = substitute_text(text, variables),
        Value::Array(items) => {
            for item in items {
                substitute_variables(item, variables);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                substitute_variables(item, variables);
            }
        }
        _ => {}
    }
}

/// Pulls the savable value out of a step result: native actions return the result
/// directly, browser actions wrap it in an envelope.
#[cfg(test)]
#[allow(dead_code)]
fn extract_saved_value(value: &Value) -> Option<String> {
    for key in ["value", "text", "label", "url", "title"] {
        if let Some(found) = value.get(key).and_then(Value::as_str) {
            return Some(found.to_string());
        }
        if let Some(found) = value
            .get("result")
            .and_then(|result| result.get(key))
            .and_then(Value::as_str)
        {
            return Some(found.to_string());
        }
    }
    None
}

/// What a request_user step is waiting for the user to decide. Persisted so the
/// approval command can grant a durable permission for future runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingApproval {
    step_id: String,
    application: String,
    effect: String,
    intent: String,
    reason: String,
}

fn approval_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    Ok(checkpoints_dir(app)?.join(format!("{run_id}.approval.json")))
}

#[tauri::command]
fn approve_run_step(app: AppHandle, run_id: String) -> Result<(), String> {
    let path = approval_path(&app, &run_id)?;
    let contents = fs::read_to_string(&path)
        .map_err(|_| "This run is not waiting for approval.".to_string())?;
    let pending: PendingApproval =
        serde_json::from_str(&contents).map_err(|error| error.to_string())?;
    // A durable grant covers future runs of this application and effect kind...
    let _ = grant_permission(
        app.clone(),
        pending.application.clone(),
        vec![pending.effect.clone()],
        Vec::new(),
    );
    // ...and a one-step override covers this step right now, including
    // unknown-effect steps that no grant could authorize. hard_deny steps never
    // reach this state, so they can never be overridden.
    let state = app.state::<RuntimeState>();
    state
        .approved_overrides
        .lock()
        .map_err(|_| "Approval state is unavailable.")?
        .insert(run_id.clone(), pending.step_id);
    state
        .run_controls
        .lock()
        .map_err(|_| "Run control state is unavailable.")?
        .insert(run_id.clone(), "running".into());
    let _ = fs::remove_file(&path);
    Ok(())
}

fn native_host_executable(app: &AppHandle) -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("ALFRED_WINDOWS_HOST_PATH") {
        return Ok(PathBuf::from(path));
    }
    let name = if cfg!(windows) {
        "alfred-windows-host.exe"
    } else {
        "alfred-windows-host"
    };
    let mut candidates = Vec::new();
    if let Ok(resource) = app.path().resource_dir() {
        candidates.push(resource.join(name));
        candidates.push(resource.join("native/windows-host").join(name));
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            candidates.push(parent.join(name));
        }
    }
    candidates.into_iter().find(|path| path.exists()).ok_or_else(|| "The Alfred Windows automation host is not installed. Run the Windows setup or repair command.".to_string())
}

fn execute_native_action_inner(
    app: &AppHandle,
    state: &RuntimeState,
    request: ActionRequest,
    method: String,
    timeout: Duration,
    approval_override: bool,
) -> Result<Value, String> {
    let method_name = method.clone();
    let decision = evaluate_action(app.clone(), request.clone())?;
    // hard_deny is absolute; only request_user can be overridden by the explicit
    // one-step approval the user grants at the mid-run waiting prompt.
    let overridden = approval_override && decision.decision == "request_user";
    if decision.decision != "allow" && !overridden {
        return Err(format!("{}: {}", decision.decision, decision.reason));
    }
    if !cfg!(windows) {
        return Err("This recorded step requires the Windows automation host. macOS native execution is not enabled for this step.".into());
    }
    let mut guard = state
        .native_host
        .lock()
        .map_err(|_| "Native host state is unavailable.".to_string())?;
    let needs_start = guard
        .as_mut()
        .map(|host| host.child.try_wait().ok().flatten().is_some())
        .unwrap_or(true);
    if needs_start {
        // Drop the previous host (if any) so its worker thread, pipes, and
        // child process are reaped before we spawn a replacement.
        *guard = None;
        *guard = Some(spawn_native_host(app)?);
    }
    let host = guard
        .as_mut()
        .ok_or_else(|| "Native host failed to start.".to_string())?;
    let message = serde_json::json!({
        "id": if request.workflow_step.is_empty() { Uuid::new_v4().to_string() } else { request.workflow_step.clone() },
        "method": method, "capabilityToken": host.capability_token, "params": request.payload.clone().unwrap_or_else(|| serde_json::json!({})),
        "application": request.application, "intent": request.intent, "target": request.target_label
    });
    if host.to_host.send(message.to_string()).is_err() {
        *guard = None;
        return Err(
            "The native host is not responding; it will restart on the next action.".into(),
        );
    }
    let response = match host.from_host.recv_timeout(timeout) {
        Ok(Ok(line)) => line,
        Ok(Err(error)) => {
            let detail = host
                .last_stderr
                .lock()
                .ok()
                .filter(|line| !line.is_empty())
                .map(|line| format!("{error} Host: {line}"))
                .unwrap_or(error);
            let _ = host.child.kill();
            *guard = None;
            return Err(detail);
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // A hung target application must fail this one step, never freeze
            // every run that shares the host. Kill and respawn on demand.
            let _ = host.child.kill();
            *guard = None;
            return Err(format!(
                "The native action timed out after {} ms; the host was restarted.",
                timeout.as_millis()
            ));
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            *guard = None;
            return Err("The native host stopped responding.".into());
        }
    };
    let value: Value = serde_json::from_str(&response).map_err(|error| error.to_string())?;
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Native action failed.")
            .to_string();
        append_run_log(
            app,
            &request.run_id,
            serde_json::json!({
                "kind": "tool",
                "channel": "native",
                "method": method_name,
                "application": request.application,
                "ok": false,
                "error": error,
                "payload": compact_log_value(request.payload.as_ref().unwrap_or(&Value::Null))
            }),
        );
        return Err(error);
    }
    let result = value.get("result").cloned().unwrap_or(Value::Null);
    append_run_log(
        app,
        &request.run_id,
        serde_json::json!({
            "kind": "tool",
            "channel": "native",
            "method": method_name,
            "application": request.application,
            "ok": true,
            "result": compact_log_value(&result)
        }),
    );
    Ok(result)
}

/// Re-resolve a recorded application name to the process that owns its window
/// right now. Recorded PIDs go stale and can even be reused by other programs,
/// so replay always re-binds identity through this lookup.
/// Per-attempt PID rebinding applies to every native step EXCEPT launches: the
/// point of launchApplication is that the application is not running yet, so
/// pre-resolving it would always fail and the launch would never happen.
fn needs_process_resolution(kind: &str, application: &str) -> bool {
    !matches!(
        kind,
        "launchApplication" | "listApplications" | "listInstalledApplications" | "resolveApplication"
    ) && application != "Alfred"
}

fn resolve_application_process_id(
    app: &AppHandle,
    state: &RuntimeState,
    application: &str,
) -> Result<i64, String> {
    let request = ActionRequest {
        protocol_version: protocol_version(),
        run_id: "resolve".into(),
        workflow_step: String::new(),
        application: application.into(),
        intent: format!("locate the running window for {application}"),
        effect: "observe".into(),
        target_label: Some(application.into()),
        payload: Some(serde_json::json!({ "name": application })),
    };
    let value = execute_native_action_inner(
        app,
        state,
        request,
        "resolveApplication".into(),
        Duration::from_secs(10),
        false,
    )?;
    value
        .get("processId")
        .and_then(Value::as_i64)
        .ok_or_else(|| format!("Could not resolve a running window for {application}."))
}

#[tauri::command]
fn execute_native_action(
    app: AppHandle,
    state: State<'_, RuntimeState>,
    request: ActionRequest,
    method: String,
) -> Result<Value, String> {
    execute_native_action_inner(
        &app,
        &state,
        request,
        method,
        Duration::from_secs(30),
        false,
    )
}

fn checkpoint_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    Ok(checkpoints_dir(app)?.join(format!("{run_id}.json")))
}

#[cfg(test)]
#[allow(dead_code)]
fn variables_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    Ok(checkpoints_dir(app)?.join(format!("{run_id}.variables.json")))
}

#[cfg(test)]
#[allow(dead_code)]
fn load_variables(app: &AppHandle, run_id: &str) -> HashMap<String, String> {
    variables_path(app, run_id)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(dead_code)]
fn save_variables(app: &AppHandle, run_id: &str, variables: &HashMap<String, String>) {
    if let Ok(path) = variables_path(app, run_id) {
        let _ = write_json(&path, variables);
    }
}

/// Evaluates one step condition against live application state. Native steps use a
/// UIA lookup in the resolved process; browser steps use a DOM observation of the
/// pinned (or active) tab. `${variable}` placeholders are resolved first.
#[cfg(test)]
#[allow(dead_code)]
fn evaluate_step_condition(
    app: &AppHandle,
    application: &str,
    is_browser: bool,
    condition: &StepCondition,
    pinned_tab: Option<i64>,
    variables: &HashMap<String, String>,
) -> Result<bool, String> {
    let found = if is_browser {
        let mut params = serde_json::json!({});
        if let (Some(tab), Value::Object(ref mut map)) = (pinned_tab, &mut params) {
            map.insert("tabId".to_string(), Value::from(tab));
        }
        let value = send_browser_command_inner(
            app.clone(),
            BrowserCommand {
                id: "condition-check".into(),
                method: "observe".into(),
                effect: "observe".into(),
                intent: "check page state before continuing".into(),
                target_label: None,
                params,
                run_id: None,
            },
            false,
        )?;
        let result = value.get("result").cloned().unwrap_or(Value::Null);
        let url_ok = condition
            .url_contains
            .as_deref()
            .map(|needle| {
                result
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&substitute_text(needle, variables).to_lowercase())
            })
            .unwrap_or(true);
        let name_ok = condition
            .name
            .as_deref()
            .map(|needle| {
                let needle = substitute_text(needle, variables).to_lowercase();
                result
                    .get("elements")
                    .and_then(Value::as_array)
                    .map(|elements| {
                        elements.iter().any(|element| {
                            element
                                .get("label")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_lowercase()
                                .contains(&needle)
                        })
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(true);
        url_ok && name_ok
    } else {
        let state = app.state::<RuntimeState>();
        let pid = resolve_application_process_id(app, &state, application)?;
        let mut params = serde_json::json!({ "processId": pid });
        if let Value::Object(ref mut map) = params {
            if let Some(id) = &condition.automation_id {
                map.insert("automationId".into(), Value::from(id.as_str()));
            }
            if let Some(name) = &condition.name {
                map.insert("name".into(), Value::from(substitute_text(name, variables)));
            }
            if let Some(control_type) = &condition.control_type {
                map.insert("controlType".into(), Value::from(control_type.as_str()));
            }
        }
        let request = ActionRequest {
            protocol_version: protocol_version(),
            run_id: "condition-check".into(),
            workflow_step: String::new(),
            application: application.into(),
            intent: "check application state before continuing".into(),
            effect: "observe".into(),
            target_label: condition.name.clone(),
            payload: Some(params),
        };
        let value = execute_native_action_inner(
            app,
            &state,
            request,
            "findElement".into(),
            Duration::from_secs(10),
            false,
        )?;
        value.get("found").and_then(Value::as_bool).unwrap_or(false)
    };
    Ok(if condition.absent { !found } else { found })
}

#[cfg(test)]
#[allow(dead_code)]
enum WaitOutcome {
    Satisfied,
    TimedOut,
    Stopped,
}

/// Polls a condition until it holds or the deadline passes. Transient lookup
/// errors (busy app, restarting host) keep the wait alive; stop/pause from the
/// user are honored between polls.
#[cfg(test)]
#[allow(dead_code)]
async fn wait_for_condition(
    app: &AppHandle,
    run_id: &str,
    application: &str,
    is_browser: bool,
    condition: &StepCondition,
    timeout: Duration,
    pinned_tab: Option<i64>,
    variables: &HashMap<String, String>,
) -> WaitOutcome {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match run_mode(app, run_id).as_str() {
            "stop" => return WaitOutcome::Stopped,
            "paused" | "waiting" => {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                continue;
            }
            _ => {}
        }
        if let Ok(true) = evaluate_step_condition(
            app,
            application,
            is_browser,
            condition,
            pinned_tab,
            variables,
        ) {
            return WaitOutcome::Satisfied;
        }
        if std::time::Instant::now() >= deadline {
            return WaitOutcome::TimedOut;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

// Keyboard focus is a machine-global resource, so only one workflow may drive the
// computer at a time. The lock is a file (not just in-process state) because the
// Windows Task Scheduler launches a separate Alfred process. A heartbeat keeps the
// lock fresh; a lock whose heartbeat stopped is treated as abandoned after a crash.
const RUN_LOCK_STALE_MINUTES: i64 = 10;

fn run_lock_path(app: &AppHandle) -> Result<PathBuf, String> {
    Ok(app_data_dir(app)?.join("run.lock"))
}

fn read_run_lock(path: &Path) -> Option<(String, DateTime<Utc>)> {
    let contents = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&contents).ok()?;
    let run_id = value.get("runId")?.as_str()?.to_string();
    let updated = value
        .get("updatedAt")?
        .as_str()?
        .parse::<DateTime<Utc>>()
        .ok()?;
    Some((run_id, updated))
}

fn write_run_lock(path: &Path, run_id: &str) {
    let body = serde_json::json!({ "runId": run_id, "updatedAt": Utc::now().to_rfc3339() });
    let _ = atomic_write(path, body.to_string().as_bytes());
}

fn try_acquire_run_lock(path: &Path, run_id: &str) -> Result<(), String> {
    if let Some((active_id, updated)) = read_run_lock(path) {
        let fresh = Utc::now() - updated < chrono::Duration::minutes(RUN_LOCK_STALE_MINUTES);
        if fresh && active_id != run_id {
            return Err(
                "Another Alfred run is currently driving this computer. Stop it or wait for it to finish."
                    .into(),
            );
        }
    }
    write_run_lock(path, run_id);
    Ok(())
}

fn release_run_lock(path: &Path, run_id: &str) {
    if let Some((active_id, _)) = read_run_lock(path) {
        if active_id == run_id {
            let _ = fs::remove_file(path);
        }
    }
}

fn save_checkpoint(app: &AppHandle, checkpoint: &RunCheckpoint) -> Result<(), String> {
    write_json(&checkpoint_path(app, &checkpoint.run_id)?, checkpoint)
}

#[tauri::command]
fn get_checkpoint(app: AppHandle, run_id: String) -> Result<Option<RunCheckpoint>, String> {
    let path = checkpoint_path(&app, &run_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(|error| error.to_string())?;
    let checkpoint = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    Ok(Some(checkpoint))
}

fn goal_run_steps_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    let directory = app_data_dir(app)?.join("goal-run-steps");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory.join(format!("{run_id}.json")))
}

fn append_goal_run_step(app: &AppHandle, run_id: &str, step: &WorkflowStep) {
    let Ok(path) = goal_run_steps_path(app, run_id) else {
        return;
    };
    let mut steps: Vec<WorkflowStep> = read_json_or_default(&path).unwrap_or_default();
    let duplicate_launch = step.kind == "launchApplication"
        && steps.iter().any(|saved| {
            saved.kind == "launchApplication"
                && saved.application.as_deref() == step.application.as_deref()
        });
    let duplicate_last = steps.last().is_some_and(|saved| {
        saved.kind == step.kind
            && saved.application == step.application
            && saved.target_label == step.target_label
            && saved.payload == step.payload
    });
    if !duplicate_launch && !duplicate_last {
        steps.push(step.clone());
        let _ = write_json(&path, &steps);
    }
}

#[tauri::command]
fn complete_goal_run(app: AppHandle, run_id: String) -> Result<RunCheckpoint, String> {
    let current = get_checkpoint(app.clone(), run_id.clone())?
        .ok_or_else(|| "The run checkpoint was not found.".to_string())?;
    if current.status == "failed" || current.status == "stopped" {
        return Err(format!(
            "A {} run cannot be marked complete.",
            current.status
        ));
    }
    let checkpoint = RunCheckpoint {
        run_id: run_id.clone(),
        workflow_id: current.workflow_id,
        next_step_index: current.next_step_index,
        status: "completed".into(),
        error: None,
        updated_at: Utc::now(),
    };
    save_checkpoint(&app, &checkpoint)?;
    if let Ok(Some(mut memory)) = get_goal_run_memory(app.clone(), run_id.clone()) {
        memory.status = "completed".into();
        memory.completion_summary = Some("The user verified the outcome on the desktop.".into());
        memory.completion_evidence = vec!["User-confirmed visible outcome".into()];
        memory.pending_action = None;
        let _ = save_goal_run_memory(&app, &mut memory);
    }
    let _ = app.emit(
        "alfred://run-event",
        RunEvent {
            run_id: run_id.clone(),
            sequence: checkpoint.next_step_index,
            step_id: "user-completed".into(),
            title: "Goal completed".into(),
            detail: "You confirmed that the requested outcome is complete.".into(),
            application: "Alfred".into(),
            status: "completed".into(),
            progress: 100,
            evidence_data_url: None,
            timestamp: checkpoint.updated_at,
        },
    );
    if let Ok(mut controls) = app.state::<RuntimeState>().run_controls.lock() {
        controls.insert(run_id, "stop".into());
    }
    Ok(checkpoint)
}

#[tauri::command]
fn save_goal_run_as_workflow(
    app: AppHandle,
    library_path: String,
    run_id: String,
    name: String,
    goal: String,
) -> Result<Workflow, String> {
    let checkpoint = get_checkpoint(app.clone(), run_id.clone())?
        .ok_or_else(|| "The run checkpoint was not found.".to_string())?;
    if checkpoint.status != "completed" {
        return Err("Finish the run before saving it as a workflow.".into());
    }
    let path = goal_run_steps_path(&app, &run_id)?;
    let mut steps: Vec<WorkflowStep> = read_json_or_default(&path)?;
    steps.retain(|step| validate_workflow_step(step).is_ok());
    if steps.is_empty() {
        return Err("This run did not capture any reusable actions.".into());
    }
    for step in &mut steps {
        step.id = Uuid::new_v4().to_string();
    }
    let mut required_apps = Vec::new();
    for application in steps.iter().filter_map(|step| step.application.clone()) {
        if application != "Alfred" && !required_apps.contains(&application) {
            required_apps.push(application);
        }
    }
    let now = Utc::now();
    let planner_provider =
        get_goal_run_memory(app.clone(), run_id.clone())?.map(|memory| memory.provider);
    let workflow = Workflow {
        id: Uuid::new_v4().to_string(),
        name: name.trim().to_string(),
        goal: goal.trim().to_string(),
        version: "1.0.0".into(),
        created_at: now,
        updated_at: now,
        status: "ready".into(),
        planner_provider,
        required_apps,
        steps,
    };
    if workflow.name.is_empty() || workflow.goal.is_empty() {
        return Err("A saved workflow needs both a name and a goal.".into());
    }
    save_workflow(&workflow_path(&library_path, &workflow), &workflow)?;
    Ok(workflow)
}

fn run_mode(app: &AppHandle, run_id: &str) -> String {
    app.state::<RuntimeState>()
        .run_controls
        .lock()
        .ok()
        .and_then(|map| map.get(run_id).cloned())
        .unwrap_or_else(|| "stop".into())
}

/// Waits while a run is paused. Returns true when the run must stop.
async fn wait_if_paused(app: &AppHandle, run_id: &str) -> bool {
    loop {
        match run_mode(app, run_id).as_str() {
            "stop" => return true,
            "paused" => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            _ => return false,
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
#[allow(dead_code)]
fn fail_run_step(
    app: &AppHandle,
    run_id: &str,
    workflow_id: &str,
    index: usize,
    total: usize,
    step: &WorkflowStep,
    application: &str,
    error: String,
) {
    let _ = app.emit(
        "alfred://run-event",
        RunEvent {
            run_id: run_id.into(),
            sequence: index,
            step_id: step.id.clone(),
            title: step.title.clone(),
            detail: error.clone(),
            application: application.into(),
            status: "failed".into(),
            progress: (index * 100 / total) as u8,
            evidence_data_url: None,
            timestamp: Utc::now(),
        },
    );
    let _ = save_checkpoint(
        app,
        &RunCheckpoint {
            run_id: run_id.into(),
            workflow_id: workflow_id.into(),
            next_step_index: index,
            status: "failed".into(),
            error: Some(error),
            updated_at: Utc::now(),
        },
    );
}

fn stop_run(app: &AppHandle, run_id: &str, workflow_id: &str, index: usize) {
    if get_checkpoint(app.clone(), run_id.to_string())
        .ok()
        .flatten()
        .is_some_and(|checkpoint| checkpoint.status == "completed")
    {
        return;
    }
    let _ = save_checkpoint(
        app,
        &RunCheckpoint {
            run_id: run_id.into(),
            workflow_id: workflow_id.into(),
            next_step_index: index,
            status: "stopped".into(),
            error: None,
            updated_at: Utc::now(),
        },
    );
}

/// Parks the run when the policy engine returns request_user: records what is
/// pending, emits a "waiting" event the UI turns into an approval prompt, then
/// polls until the user approves (approve_run_step flips the mode back to running
/// with a grant + one-step override) or stops the run. Returns true if approved.
/// Headless scheduled runs never get an answer; their monitor exits non-zero on
/// the "waiting" checkpoint, which is the intended fail-closed behavior.
#[allow(clippy::too_many_arguments)]
async fn park_run_for_approval(
    app: &AppHandle,
    run_id: &str,
    workflow_id: &str,
    index: usize,
    progress: u8,
    step: &WorkflowStep,
    application: &str,
    error: String,
) -> bool {
    let reason = error.trim_start_matches("request_user:").trim().to_string();
    let pending = PendingApproval {
        step_id: step.id.clone(),
        application: application.into(),
        effect: step.effect.clone(),
        intent: step.intent.clone().unwrap_or_else(|| step.kind.clone()),
        reason: reason.clone(),
    };
    if let Ok(path) = approval_path(app, run_id) {
        let _ = write_json(&path, &pending);
    }
    if let Ok(mut controls) = app.state::<RuntimeState>().run_controls.lock() {
        controls.insert(run_id.into(), "waiting".into());
    }
    let _ = app.emit(
        "alfred://run-event",
        RunEvent {
            run_id: run_id.into(),
            sequence: index,
            step_id: step.id.clone(),
            title: step.title.clone(),
            detail: format!("Approval needed: {reason}"),
            application: application.into(),
            status: "waiting".into(),
            progress,
            evidence_data_url: None,
            timestamp: Utc::now(),
        },
    );
    let _ = save_checkpoint(
        app,
        &RunCheckpoint {
            run_id: run_id.into(),
            workflow_id: workflow_id.into(),
            next_step_index: index,
            status: "waiting".into(),
            error: Some(reason),
            updated_at: Utc::now(),
        },
    );
    loop {
        match run_mode(app, run_id).as_str() {
            "running" => return true,
            "stop" => {
                if let Ok(path) = approval_path(app, run_id) {
                    let _ = fs::remove_file(path);
                }
                stop_run(app, run_id, workflow_id, index);
                return false;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
    }
}

/// Async for the same reason as start_goal_run: the Windows app-resolution
/// preflight below performs native-host round-trips that must not block the
/// WebView's main thread.
#[cfg(test)]
#[allow(dead_code)]
async fn start_workflow_run(
    app: AppHandle,
    library_path: String,
    workflow_id: String,
    resume_run_id: Option<String>,
) -> Result<String, String> {
    let (_, workflow) = load_workflow(&library_path, &workflow_id)?;
    for step in &workflow.steps {
        validate_workflow_step(step)?;
    }
    let run_id = resume_run_id.unwrap_or_else(|| Uuid::new_v4().to_string());
    let start_index = get_checkpoint(app.clone(), run_id.clone())?
        .map(|item| item.next_step_index)
        .unwrap_or(0);
    let lock_path = run_lock_path(&app)?;
    {
        let state = app.state::<RuntimeState>();
        let mut controls = state
            .run_controls
            .lock()
            .map_err(|_| "Run control state is unavailable.")?;
        if controls.contains_key(&run_id) {
            return Err("This run is already active.".into());
        }
        controls.insert(run_id.clone(), "running".into());
    }
    let start = (|| {
        try_acquire_run_lock(&lock_path, &run_id)?;
        // Preflight: every application the workflow needs must be running before the
        // first step, so a missing app fails fast with a clear message.
        if cfg!(windows) {
            let runtime = app.state::<RuntimeState>();
            for required in &workflow.required_apps {
                if required == "Alfred" || required == "Installed browser" {
                    continue;
                }
                resolve_application_process_id(&app, &runtime, required)
                    .map_err(|error| format!("{required} is not ready: {error}"))?;
            }
        }
        save_checkpoint(
            &app,
            &RunCheckpoint {
                run_id: run_id.clone(),
                workflow_id: workflow_id.clone(),
                next_step_index: start_index,
                status: "running".into(),
                error: None,
                updated_at: Utc::now(),
            },
        )
    })();
    if let Err(error) = start {
        release_run_lock(&lock_path, &run_id);
        if let Ok(mut controls) = app.state::<RuntimeState>().run_controls.lock() {
            controls.remove(&run_id);
        }
        return Err(error);
    }
    let emitted_run = run_id.clone();
    let app_for_run = app.clone();
    tauri::async_runtime::spawn(async move {
        drive_workflow_run(
            app_for_run.clone(),
            emitted_run.clone(),
            workflow,
            start_index,
        )
        .await;
        release_run_lock(&lock_path, &emitted_run);
        if let Ok(mut controls) = app_for_run.state::<RuntimeState>().run_controls.lock() {
            controls.remove(&emitted_run);
        }
    });
    Ok(run_id)
}

#[cfg(test)]
#[allow(dead_code)]
async fn drive_workflow_run(
    app: AppHandle,
    run_id: String,
    workflow: Workflow,
    start_index: usize,
) {
    let total = workflow.steps.len().max(1);
    let lock_path = run_lock_path(&app).ok();
    // Browser steps in one run stick to the tab Alfred last used, so the user or
    // another application changing the active tab mid-run cannot redirect actions.
    let mut pinned_tab: Option<i64> = None;
    // Values captured by saveAs steps; persisted so checkpoint resumes keep them.
    let mut variables = load_variables(&app, &run_id);
    for (index, step) in workflow.steps.iter().enumerate().skip(start_index) {
        if wait_if_paused(&app, &run_id).await {
            stop_run(&app, &run_id, &workflow.id, index);
            return;
        }
        if let Some(path) = &lock_path {
            write_run_lock(path, &run_id);
        }
        let application = step.application.clone().unwrap_or_else(|| "Alfred".into());
        let is_browser = step.kind.starts_with("browser.");
        let attempts = 1 + step.retries as u32;
        let timeout = Duration::from_millis(step.timeout_ms.clamp(1_000, 120_000));
        let _ = app.emit(
            "alfred://run-event",
            RunEvent {
                run_id: run_id.clone(),
                sequence: index,
                step_id: step.id.clone(),
                title: step.title.clone(),
                detail: "Checking permission and handing the action to the trusted host.".into(),
                application: application.clone(),
                status: "running".into(),
                progress: (index * 100 / total) as u8,
                evidence_data_url: None,
                timestamp: Utc::now(),
            },
        );
        // Precondition: wait for the target state before acting at all.
        if let Some(wait_for) = &step.wait_for {
            let label = wait_for
                .name
                .clone()
                .or_else(|| wait_for.url_contains.clone())
                .unwrap_or_else(|| "the target state".into());
            let _ = app.emit(
                "alfred://run-event",
                RunEvent {
                    run_id: run_id.clone(),
                    sequence: index,
                    step_id: step.id.clone(),
                    title: step.title.clone(),
                    detail: format!("Waiting for {label}."),
                    application: application.clone(),
                    status: "running".into(),
                    progress: (index * 100 / total) as u8,
                    evidence_data_url: None,
                    timestamp: Utc::now(),
                },
            );
            match wait_for_condition(
                &app,
                &run_id,
                &application,
                is_browser,
                wait_for,
                timeout,
                pinned_tab,
                &variables,
            )
            .await
            {
                WaitOutcome::Satisfied => {}
                WaitOutcome::TimedOut => {
                    fail_run_step(
                        &app,
                        &run_id,
                        &workflow.id,
                        index,
                        total,
                        step,
                        &application,
                        format!(
                            "Precondition \"{label}\" was not met within {} ms.",
                            timeout.as_millis()
                        ),
                    );
                    return;
                }
                WaitOutcome::Stopped => {
                    stop_run(&app, &run_id, &workflow.id, index);
                    return;
                }
            }
        }
        let mut outcome: Option<Value> = None;
        let mut last_error = String::new();
        let mut skipped = false;
        let mut attempt = 1u32;
        while attempt <= attempts {
            if wait_if_paused(&app, &run_id).await {
                stop_run(&app, &run_id, &workflow.id, index);
                return;
            }
            let mut payload = step
                .payload
                .clone()
                .unwrap_or_else(|| serde_json::json!({}));
            substitute_variables(&mut payload, &variables);
            let mut target_label = step.target_label.clone();
            if let Some(label) = &mut target_label {
                *label = substitute_text(label, &variables);
            }
            // Idempotent resume: when the desired end state already holds (a
            // previous attempt applied it but the response was lost), skip the
            // action instead of applying it twice.
            if step.effect != "observe" {
                if let Some(expect) = &step.expect {
                    if let Ok(true) = evaluate_step_condition(
                        &app,
                        &application,
                        is_browser,
                        expect,
                        pinned_tab,
                        &variables,
                    ) {
                        skipped = true;
                        outcome = Some(serde_json::json!({ "skipped": true }));
                        break;
                    }
                }
            }
            // Every attempt re-resolves the application to a live process, so a
            // stale recorded PID can never steer input into the wrong window.
            if !is_browser && cfg!(windows) && needs_process_resolution(&step.kind, &application) {
                let runtime = app.state::<RuntimeState>();
                match resolve_application_process_id(&app, &runtime, &application) {
                    Ok(pid) => {
                        if let Value::Object(ref mut map) = payload {
                            map.insert("processId".into(), Value::from(pid));
                        }
                    }
                    Err(error) => {
                        last_error = error;
                        attempt += 1;
                        if attempt <= attempts {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        continue;
                    }
                }
            }
            if is_browser {
                if let (Some(tab), Value::Object(map)) = (pinned_tab, &mut payload) {
                    map.entry("tabId".to_string()).or_insert(Value::from(tab));
                }
            }
            let approved = app
                .state::<RuntimeState>()
                .approved_overrides
                .lock()
                .ok()
                .map(|overrides| overrides.get(&run_id) == Some(&step.id))
                .unwrap_or(false);
            let floored_effect = effective_effect_for(
                &step.kind,
                &step.effect,
                step.target_label.as_deref(),
                step.payload.as_ref(),
            );
            let request = ActionRequest {
                protocol_version: protocol_version(),
                run_id: run_id.clone(),
                workflow_step: step.id.clone(),
                application: application.clone(),
                intent: step.intent.clone().unwrap_or_else(|| step.kind.clone()),
                effect: floored_effect.clone(),
                target_label,
                payload: Some(payload.clone()),
            };
            let result = if is_browser {
                send_browser_command_inner(
                    app.clone(),
                    BrowserCommand {
                        id: step.id.clone(),
                        method: step.kind.trim_start_matches("browser.").into(),
                        effect: floored_effect,
                        intent: step.intent.clone().unwrap_or_else(|| step.title.clone()),
                        target_label: step.target_label.clone(),
                        params: payload,
                        run_id: Some(run_id.clone()),
                    },
                    approved,
                )
            } else {
                let runtime = app.state::<RuntimeState>();
                execute_native_action_inner(
                    &app,
                    &runtime,
                    request,
                    step.kind.clone(),
                    timeout,
                    approved,
                )
            };
            match result {
                Ok(value) => {
                    // Postcondition: confirm the action actually reached the
                    // desired state before calling the step complete.
                    if let Some(expect) = &step.expect {
                        match wait_for_condition(
                            &app,
                            &run_id,
                            &application,
                            is_browser,
                            expect,
                            timeout,
                            pinned_tab,
                            &variables,
                        )
                        .await
                        {
                            WaitOutcome::Satisfied => {
                                outcome = Some(value);
                                break;
                            }
                            WaitOutcome::Stopped => {
                                stop_run(&app, &run_id, &workflow.id, index);
                                return;
                            }
                            WaitOutcome::TimedOut => {
                                last_error =
                                    "The action ran but the expected state did not appear in time."
                                        .into();
                                attempt += 1;
                                if attempt <= attempts {
                                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                                }
                                continue;
                            }
                        }
                    }
                    outcome = Some(value);
                    break;
                }
                Err(error) if error.starts_with("request_user") => {
                    // Park until the user approves or stops; approval re-attempts
                    // the step without consuming one of its retries.
                    let approved_now = park_run_for_approval(
                        &app,
                        &run_id,
                        &workflow.id,
                        index,
                        (index * 100 / total.max(1)) as u8,
                        step,
                        &application,
                        error,
                    )
                    .await;
                    if !approved_now {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    // hard_deny is deterministic; retrying cannot help it.
                    let retryable = !error.starts_with("hard_deny");
                    last_error = error;
                    attempt += 1;
                    if !retryable || attempt > attempts {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        let Some(value) = outcome else {
            fail_run_step(
                &app,
                &run_id,
                &workflow.id,
                index,
                total,
                step,
                &application,
                format!("{last_error} (after {attempts} attempt(s))"),
            );
            return;
        };
        // A one-step approval override is consumed with its step.
        if let Ok(mut overrides) = app.state::<RuntimeState>().approved_overrides.lock() {
            if overrides.get(&run_id) == Some(&step.id) {
                overrides.remove(&run_id);
            }
        }
        if let Some(name) = &step.save_as {
            if let Some(saved) = extract_saved_value(&value) {
                variables.insert(name.clone(), saved);
                save_variables(&app, &run_id, &variables);
            }
        }
        if is_browser {
            if let Some(tab) = value
                .get("result")
                .and_then(|result| result.get("tabId"))
                .and_then(Value::as_i64)
            {
                pinned_tab = Some(tab);
            }
        }
        let direct = value
            .get("base64")
            .and_then(Value::as_str)
            .map(|data| format!("data:image/png;base64,{data}"));
        let nested = value
            .get("result")
            .and_then(|item| item.get("dataUrl"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let detail = if skipped {
            "Already in the desired state; action skipped (idempotent resume).".to_string()
        } else if let Some(name) = &step.save_as {
            format!("Action completed; value saved as ${{{name}}}. Checkpoint saved.")
        } else {
            "Action completed and checkpoint saved.".to_string()
        };
        let _ = app.emit(
            "alfred://run-event",
            RunEvent {
                run_id: run_id.clone(),
                sequence: index,
                step_id: step.id.clone(),
                title: step.title.clone(),
                detail,
                application: application.clone(),
                status: "completed".into(),
                progress: (((index + 1) * 100) / total) as u8,
                evidence_data_url: direct.or(nested),
                timestamp: Utc::now(),
            },
        );
        let _ = save_checkpoint(
            &app,
            &RunCheckpoint {
                run_id: run_id.clone(),
                workflow_id: workflow.id.clone(),
                next_step_index: index + 1,
                status: "running".into(),
                error: None,
                updated_at: Utc::now(),
            },
        );
    }
    let _ = save_checkpoint(
        &app,
        &RunCheckpoint {
            run_id: run_id.clone(),
            workflow_id: workflow.id,
            next_step_index: workflow.steps.len(),
            status: "completed".into(),
            error: None,
            updated_at: Utc::now(),
        },
    );
}

/// One reply from the planner: the next action, a completion signal, or a plan
/// outline. Deliberately mirrors the workflow-step shape so goal actions flow
/// through the same policy gate, approval parking, and executors as recorded
/// steps. Aliases cover common CLI/model drift (snake_case, `method` vs `kind`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlannerReply {
    #[serde(default)]
    done: bool,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    /// Set only by the explicit completion-review turn. A normal `done` is a
    /// claim, not a terminal state.
    #[serde(default)]
    verified: Option<bool>,
    #[serde(default)]
    evidence: Option<Vec<String>>,
    #[serde(default, alias = "method", alias = "action")]
    kind: Option<String>,
    #[serde(default, alias = "app")]
    application: Option<String>,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    effect: Option<String>,
    #[serde(default, alias = "target_label", alias = "target")]
    target_label: Option<String>,
    #[serde(default, alias = "params", alias = "arguments", alias = "args")]
    payload: Option<Value>,
    /// Multi-phase goals: an outline the planner can set or revise instead of
    /// acting. The loop pins it into every later prompt as CURRENT PLAN.
    #[serde(default)]
    plan: Option<Vec<String>>,
}

fn accept_planner_reply(mut reply: PlannerReply) -> Option<PlannerReply> {
    if let Some(kind) = reply.kind.as_mut() {
        *kind = kind.trim().to_string();
        if kind.is_empty() {
            reply.kind = None;
        }
    }
    let has_plan = reply
        .plan
        .as_ref()
        .map(|plan| !plan.is_empty())
        .unwrap_or(false);
    (reply.done || reply.kind.is_some() || has_plan).then_some(reply)
}

/// Windows `cmd.exe` and many CLI argv paths choke on huge planner prompts
/// (browser.read previews + skill + history). Cap the desktop-state section so
/// Grok/Cursor still get a usable argv; they can browser.read for more text.
fn compact_planner_observations(observations: &str) -> String {
    const MAX_CHARS: usize = 3500;
    let trimmed = observations.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        return trimmed.to_string();
    }
    let kept: String = trimmed.chars().take(MAX_CHARS).collect();
    format!("{kept}\n…(observation truncated; call browser.read or browser.scroll for more page content)")
}

/// Grok/Cursor take the prompt as a CLI argument. Beyond ~6–8k characters Windows
/// command lines truncate or fail, producing empty/garbled stream output. Spill
/// the full prompt to a temp file and pass a short instruction that points at it.
fn materialize_planner_prompt(
    app: &AppHandle,
    provider: &str,
    prompt: &str,
) -> Result<(String, Option<PathBuf>), String> {
    const MAX_ARGV_PROMPT: usize = 5500;
    // Path-based vision providers must see screenshot paths inside a file, not
    // a truncated Windows argv. Spill whenever the prompt is large or carries
    // screenshot paths — the CLI remains one planner backend, not the loop.
    let must_spill = matches!(provider, "grok" | "cursor" | "copilot")
        && (prompt.len() > MAX_ARGV_PROMPT || prompt.contains("SCREENSHOT FILES"));
    if !must_spill {
        return Ok((prompt.to_string(), None));
    }
    let dir = app_data_dir(app)?.join("planner-prompts");
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join(format!("{}.txt", Uuid::new_v4()));
    fs::write(&path, prompt).map_err(|error| error.to_string())?;
    let short = format!(
        "You are Alfred's desktop planner. Read the FULL task file at this absolute path with your file-reading tool, then respond with EXACTLY one JSON object as that file instructs (done/kind/plan). No markdown fences, no prose outside the JSON.\n\nTASK FILE:\n{}\n\nIf you cannot read the file, reply only: {{\"done\":true,\"summary\":\"Could not read the planner task file.\"}}",
        path.display()
    );
    Ok((short, Some(path)))
}

/// Normalizes common LLM JSON shapes before deserializing into PlannerReply:
/// `method` → `kind`, snake_case keys, and stringified nested JSON payloads.
fn normalize_planner_json_value(mut value: Value) -> Value {
    if let Value::Object(map) = &mut value {
        // Promote nested action objects first: { "action": { "kind": ... } }
        // Must run before treating a string "action"/"method" as the kind name.
        if !map.contains_key("kind") {
            for key in ["action", "step", "next", "command"] {
                if matches!(map.get(key), Some(Value::Object(_))) {
                    if let Some(Value::Object(inner)) = map.remove(key) {
                        return normalize_planner_json_value(Value::Object(inner));
                    }
                }
            }
        }
        if !map.contains_key("kind") {
            if let Some(method) = map
                .get("method")
                .cloned()
                .or_else(|| map.get("action").cloned())
            {
                if method.is_string() {
                    map.insert("kind".into(), method);
                    map.remove("method");
                }
            }
        }
        if !map.contains_key("targetLabel") {
            if let Some(target) = map.remove("target_label").or_else(|| map.remove("target")) {
                map.insert("targetLabel".into(), target);
            }
        }
        if !map.contains_key("payload") {
            if let Some(params) = map
                .remove("params")
                .or_else(|| map.remove("arguments"))
                .or_else(|| map.remove("args"))
            {
                map.insert("payload".into(), params);
            }
        }
        if let Some(Value::String(raw)) = map.get("payload").cloned() {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                map.insert("payload".into(), parsed);
            }
        }
    }
    value
}

fn planner_reply_from_value(value: Value) -> Option<PlannerReply> {
    let normalized = normalize_planner_json_value(value);
    serde_json::from_value::<PlannerReply>(normalized)
        .ok()
        .and_then(accept_planner_reply)
}

/// Balanced-brace extraction of every top-level JSON object in a blob. Streaming
/// CLIs interleave events; the action may not sit at the first `{` … last `}`.
fn extract_json_objects(text: &str) -> Vec<String> {
    let mut objects = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        let start = i;
        let mut depth = 0i32;
        let mut in_string = false;
        let mut escape = false;
        while i < bytes.len() {
            let c = bytes[i];
            if in_string {
                if escape {
                    escape = false;
                } else if c == b'\\' {
                    escape = true;
                } else if c == b'"' {
                    in_string = false;
                }
            } else if c == b'"' {
                in_string = true;
            } else if c == b'{' {
                depth += 1;
            } else if c == b'}' {
                depth -= 1;
                if depth == 0 {
                    if let Ok(slice) = std::str::from_utf8(&bytes[start..=i]) {
                        objects.push(slice.to_string());
                    }
                    break;
                }
            }
            i += 1;
        }
        i += 1;
    }
    objects
}

/// Parses one candidate text into a planner reply: direct JSON first (markdown
/// fences stripped), then every balanced JSON object (prose-wrapped / stream).
fn planner_reply_from_text(text: &str) -> Option<PlannerReply> {
    let candidate = strip_json_fence(text);
    if let Ok(value) = serde_json::from_str::<Value>(candidate) {
        if let Some(reply) = planner_reply_from_value(value) {
            return Some(reply);
        }
    }
    // Prefer later objects — stream answers usually arrive last.
    for object in extract_json_objects(candidate).into_iter().rev() {
        if let Ok(value) = serde_json::from_str::<Value>(&object) {
            if let Some(reply) = planner_reply_from_value(value) {
                return Some(reply);
            }
        }
    }
    None
}

/// When the model refuses in prose (no JSON), end the goal run cleanly instead of
/// spinning three "unusable output" turns. Only triggers on clear refusal language.
fn planner_refusal_as_done(output: &str) -> Option<PlannerReply> {
    let lower = output.to_lowercase();
    let refused = [
        "i can't help",
        "i cannot help",
        "i can't assist",
        "i cannot assist",
        "i'm not able to",
        "i am not able to",
        "can't help with that",
        "cannot help with that",
        "against my guidelines",
        "against my policies",
        "not able to post",
        "cannot post",
        "can't post",
        "won't post",
        "will not post",
        "refuse to",
        "unable to comply",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    if !refused {
        return None;
    }
    let summary: String = output
        .split_whitespace()
        .take(40)
        .collect::<Vec<_>>()
        .join(" ");
    Some(PlannerReply {
        done: true,
        summary: Some(if summary.is_empty() {
            "The planner declined this goal.".into()
        } else {
            summary.chars().take(280).collect()
        }),
        ..PlannerReply::default()
    })
}

/// Distills an action result into a short history suffix so the planner can
/// reason over what it just read (browser.read/getText/getValue payloads), not
/// only that the action succeeded. Whitespace-collapsed and capped so a large
/// page dump cannot flood the next prompt. Preserves paging metadata so the
/// planner can continue with browser.read { offset }.
fn planner_result_digest(value: &Value) -> String {
    // Browser-bridge replies retain a `result` envelope, while the native host
    // is unwrapped by `execute_native_action_inner`. Digest both shapes so a
    // verified native write is visible to the next planner turn.
    let result = value
        .get("result")
        .cloned()
        .unwrap_or_else(|| value.clone());
    let mut parts = Vec::new();
    match &result {
        Value::String(text) if !text.trim().is_empty() => {
            let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let snippet: String = collapsed.chars().take(500).collect();
            parts.push(snippet);
        }
        Value::Object(map) => {
            for key in ["text", "value", "observedText", "prose"] {
                if let Some(text) = map
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|text| !text.trim().is_empty())
                {
                    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    let snippet: String = collapsed.chars().take(500).collect();
                    if !parts.iter().any(|part| part == &snippet) {
                        parts.push(snippet);
                    }
                }
            }
            if map.get("hasMore").and_then(Value::as_bool) == Some(true) {
                let next = map.get("nextOffset").and_then(Value::as_u64).unwrap_or(0);
                parts.push(format!("[hasMore nextOffset={next}]"));
            }
            if let Some(count) = map.get("count").and_then(Value::as_u64) {
                if map.get("matches").is_some() {
                    parts.push(format!("[find count={count}]"));
                }
            }
            if map.get("loginWall").and_then(Value::as_bool) == Some(true) {
                parts.push("[loginWall]".into());
            }
            if map.get("captcha").and_then(Value::as_bool) == Some(true) {
                parts.push("[captcha]".into());
            }
            if let Some(matches) = map.get("matches").and_then(Value::as_array) {
                let preview: Vec<String> = matches
                    .iter()
                    .take(5)
                    .filter_map(|item| {
                        let reference = item.get("ref")?.as_str()?;
                        let label = item.get("label").and_then(Value::as_str).unwrap_or("");
                        let short: String = label.chars().take(60).collect();
                        Some(format!("{reference}=\"{short}\""))
                    })
                    .collect();
                if !preview.is_empty() {
                    parts.push(format!("matches: {}", preview.join("; ")));
                }
            }
        }
        _ => {}
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(": {}", parts.join(" "))
    }
}

/// Grok Build's `streaming-json` mode emits the assistant answer as many
/// token events: `{"type":"text","data":"{\"done\""}` … `{"type":"text","data":"}"}`.
/// Concatenate every text `data` field (in order) to recover the full message.
/// Non-stream formats are unaffected (no such events → empty string).
fn assemble_streaming_text_chunks(output: &str) -> String {
    let mut assembled = String::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let Some(map) = value.as_object() else {
            continue;
        };
        let is_text = map
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "text" || kind == "assistant_text" || kind == "content");
        if !is_text {
            continue;
        }
        if let Some(chunk) = map.get("data").and_then(Value::as_str) {
            assembled.push_str(chunk);
        } else if let Some(chunk) = map.get("text").and_then(Value::as_str) {
            assembled.push_str(chunk);
        } else if let Some(chunk) = map
            .get("delta")
            .and_then(|delta| delta.get("text"))
            .and_then(Value::as_str)
        {
            assembled.push_str(chunk);
        }
    }
    assembled
}

fn parse_planner_action(output: &str) -> Result<PlannerReply, String> {
    // Provider CLIs wrap answers differently: bare JSON, prose around the JSON,
    // markdown fences, JSONL token streams (Grok streaming-json), or whole-message
    // envelopes (Grok --output-format json with a `text` field; Codex item.text).
    if let Some(reply) = planner_reply_from_text(output) {
        return Ok(reply);
    }
    // Reassemble token-streamed assistant text BEFORE line-level scraping.
    let streamed = assemble_streaming_text_chunks(output);
    if !streamed.trim().is_empty() {
        if let Some(reply) = planner_reply_from_text(&streamed) {
            return Ok(reply);
        }
        if let Some(reply) = planner_refusal_as_done(&streamed) {
            return Ok(reply);
        }
    }
    // Whole document as one JSON value (pretty-printed multi-line envelopes).
    if let Ok(value) = serde_json::from_str::<Value>(output.trim()) {
        if let Some(reply) = planner_reply_from_value(value.clone()) {
            return Ok(reply);
        }
        let mut embedded = Vec::new();
        collect_provider_text(&value, &mut embedded);
        for text in embedded.iter().rev() {
            if let Some(reply) = planner_reply_from_text(text) {
                return Ok(reply);
            }
        }
    }
    // Whole-stream embedded strings (JSONL event lines).
    let mut embedded_all = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            collect_provider_text(&value, &mut embedded_all);
        }
    }
    for text in embedded_all.iter().rev() {
        if let Some(reply) = planner_reply_from_text(text) {
            return Ok(reply);
        }
    }
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if let Some(reply) = planner_reply_from_text(trimmed) {
            return Ok(reply);
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            let mut embedded = Vec::new();
            collect_provider_text(&value, &mut embedded);
            for text in embedded.iter().rev() {
                if let Some(reply) = planner_reply_from_text(text) {
                    return Ok(reply);
                }
            }
        }
    }
    // Balanced objects anywhere in the raw stream (including multi-line JSON).
    for object in extract_json_objects(output).into_iter().rev() {
        if let Ok(value) = serde_json::from_str::<Value>(&object) {
            if let Some(reply) = planner_reply_from_value(value.clone()) {
                return Ok(reply);
            }
            let mut embedded = Vec::new();
            collect_provider_text(&value, &mut embedded);
            for text in embedded.iter().rev() {
                if let Some(reply) = planner_reply_from_text(text) {
                    return Ok(reply);
                }
            }
        }
    }
    if let Some(reply) = planner_refusal_as_done(output) {
        return Ok(reply);
    }
    Err("The planner did not return a usable action.".into())
}

const PLANNER_TURN_TIMEOUT_SECS: u64 = 180;

fn kill_planner_process(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    #[cfg(windows)]
    {
        let mut kill = Command::new("taskkill");
        hide_windows_console(&mut kill);
        let _ = kill
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn find_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key).and_then(Value::as_str) {
                    if !value.trim().is_empty() {
                        return Some(value.to_string());
                    }
                }
            }
            map.values()
                .find_map(|child| find_string_field(child, keys))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_string_field(child, keys)),
        _ => None,
    }
}

/// Session identifiers are emitted in different envelopes by each CLI. Keep
/// this tolerant just like action parsing, but only inspect provider-documented
/// session/thread fields (never request ids or per-event UUIDs).
fn provider_session_id_from_output(provider: &str, output: &str) -> Option<String> {
    let keys: &[&str] = if provider == "codex" {
        &["thread_id", "threadId"]
    } else {
        &["session_id", "sessionId"]
    };
    if let Ok(value) = serde_json::from_str::<Value>(output.trim()) {
        if let Some(id) = find_string_field(&value, keys) {
            return Some(id);
        }
    }
    output.lines().find_map(|line| {
        serde_json::from_str::<Value>(line.trim())
            .ok()
            .and_then(|value| find_string_field(&value, keys))
    })
}

/// Some CLIs report setup/authentication failures as ordinary stdout while
/// still returning exit code 0. Treat those diagnostics as failed turns so the
/// cockpit shows the real remedy instead of retrying an "unusable action".
fn provider_output_error(provider: &str, output: &str) -> Option<String> {
    let lower = output.to_lowercase();
    let authentication_failure = [
        "no authentication information found",
        "authentication required",
        "not logged in. please",
        "run the '/login' command",
    ]
    .iter()
    .any(|marker| lower.contains(marker));
    authentication_failure.then(|| {
        format!(
            "{provider} is not authenticated. Sign in with its CLI (or add its token in Alfred), then retry."
        )
    })
}

/// One agent-loop turn: a fresh, sandboxed provider process that resumes the
/// provider's exact conversation. Alfred's durable ledger remains authoritative,
/// and each process is independently cancellable.
async fn run_planner_turn(
    app: &AppHandle,
    run_id: &str,
    provider: &str,
    prompt: &str,
    images: &[PathBuf],
    session_id: Option<&str>,
    resume: bool,
    step_index: usize,
    progress: u8,
) -> Result<(String, Option<String>), String> {
    let (mut process, prompt_input) =
        provider_command(app, provider, prompt, images, session_id, resume)?;
    let mut child = process
        .spawn()
        .map_err(|error| format!("Could not start {provider}: {error}"))?;
    let planner_pid = child.id();
    // Providers that read the prompt from stdin (Codex) receive it now.
    if let Some(input) = prompt_input {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
    }
    let wait = child.wait_with_output();
    tokio::pin!(wait);
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(PLANNER_TURN_TIMEOUT_SECS);
    let mut last_heartbeat = started;
    loop {
        if run_mode(app, run_id) == "stop" {
            kill_planner_process(planner_pid);
            let _ = wait.await;
            return Err("stopped".into());
        }
        match tokio::time::timeout(Duration::from_millis(500), &mut wait).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = format!("{stdout}\n{stderr}");
                if !output.status.success() {
                    let detail: String = combined.trim().chars().take(600).collect();
                    let error = format!("{provider} exited with {}: {detail}", output.status);
                    append_run_log(
                        app,
                        run_id,
                        serde_json::json!({
                            "kind": "planner",
                            "provider": provider,
                            "ok": false,
                            "error": error
                        }),
                    );
                    return Err(error);
                }
                if let Some(error) = provider_output_error(provider, &combined) {
                    append_run_log(
                        app,
                        run_id,
                        serde_json::json!({
                            "kind": "planner",
                            "provider": provider,
                            "ok": false,
                            "error": error
                        }),
                    );
                    return Err(error);
                }
                let session = provider_session_id_from_output(provider, &combined);
                append_run_log(
                    app,
                    run_id,
                    serde_json::json!({
                        "kind": "planner",
                        "provider": provider,
                        "ok": true,
                        "elapsedMs": started.elapsed().as_millis() as u64,
                        "output": compact_log_value(&Value::String(combined.chars().take(1200).collect()))
                    }),
                );
                return Ok((combined, session));
            }
            Ok(Err(error)) => {
                append_run_log(
                    app,
                    run_id,
                    serde_json::json!({
                        "kind": "planner",
                        "provider": provider,
                        "ok": false,
                        "error": error.to_string()
                    }),
                );
                return Err(format!("The planner process failed: {error}"));
            }
            Err(_) => {
                let now = std::time::Instant::now();
                if now >= deadline {
                    kill_planner_process(planner_pid);
                    let _ = wait.await;
                    append_run_log(
                        app,
                        run_id,
                        serde_json::json!({
                            "kind": "planner",
                            "provider": provider,
                            "ok": false,
                            "error": "The planner did not answer within 180 seconds."
                        }),
                    );
                    return Err("The planner did not answer within 180 seconds.".into());
                }
                // A CLI turn can legitimately take minutes; keep the cockpit
                // informed instead of going silent until the timeout.
                if now.duration_since(last_heartbeat) >= Duration::from_secs(15) {
                    last_heartbeat = now;
                    emit_goal_event(
                        app,
                        run_id,
                        step_index,
                        "Planning the next action",
                        &format!(
                            "{provider} is still thinking ({}s elapsed).",
                            now.duration_since(started).as_secs()
                        ),
                        "running",
                        progress,
                    );
                }
            }
        }
    }
}

/// Compacts a mark catalog (preferred) or a legacy UIA tree into planner lines.
fn summarize_native_observation(value: &Value, application: &str, out: &mut Vec<String>) {
    if let Some(marks) = value.get("marks").and_then(Value::as_array) {
        let generation = value.get("generation").and_then(Value::as_u64).unwrap_or(0);
        let title = value.get("title").and_then(Value::as_str).unwrap_or("");
        let focused = value.get("focused").and_then(Value::as_str).unwrap_or("-");
        let dpi = value.get("dpi").and_then(Value::as_u64).unwrap_or(0);
        out.push(format!(
            "{application}  gen={generation}  dpi={dpi}  focused={focused}"
        ));
        if !title.is_empty() {
            out.push(format!("title: {title}"));
        }
        for mark in marks.iter().take(36) {
            let id = mark.get("id").and_then(Value::as_str).unwrap_or("?");
            let role = mark.get("role").and_then(Value::as_str).unwrap_or("Control");
            let name: String = mark
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            let automation_id = mark
                .get("automationId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let enabled = mark.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let chrome = mark.get("chrome").and_then(Value::as_bool).unwrap_or(false);
            let patterns = mark
                .get("patterns")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|item| item.to_ascii_lowercase())
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .unwrap_or_default();
            let mut line = format!("{id}  {role}");
            if !name.is_empty() {
                line.push_str(&format!(" \"{name}\""));
            }
            if !automation_id.is_empty() {
                line.push_str(&format!(" (id: {automation_id})"));
            }
            if !patterns.is_empty() {
                line.push_str(&format!("  {patterns}"));
            }
            if !enabled {
                line.push_str("  [disabled]");
            }
            if chrome {
                line.push_str("  chrome");
            }
            out.push(line);
        }
        if marks.is_empty() {
            out.push("(no interactive marks — call findElement {\"text\":\"...\"} or probe)".into());
        }
        if let Some(texts) = value.get("texts").and_then(Value::as_array) {
            for snippet in texts.iter().filter_map(Value::as_str).take(12) {
                let short: String = snippet.chars().take(160).collect();
                if !short.is_empty() {
                    out.push(format!("text: {short}"));
                }
            }
        }
        return;
    }
    out.push(format!("{application}:"));
    summarize_native_tree(value, out, 0);
}

/// Compacts a UIA observation tree into the lines a planner can act on.
fn summarize_native_tree(node: &Value, out: &mut Vec<String>, depth: usize) {
    if out.len() >= 40 || depth > 6 {
        return;
    }
    let control = node
        .get("controlType")
        .and_then(Value::as_str)
        .unwrap_or("");
    let name = node.get("name").and_then(Value::as_str).unwrap_or("");
    let automation_id = node
        .get("automationId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let enabled = node.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let bounds = node.get("bounds").and_then(Value::as_object);
    let interesting = matches!(
        control,
        "ControlType.Button"
            | "ControlType.Edit"
            | "ControlType.MenuItem"
            | "ControlType.ListItem"
            | "ControlType.Hyperlink"
            | "ControlType.TabItem"
            | "ControlType.ComboBox"
            | "ControlType.CheckBox"
            | "ControlType.RadioButton"
            | "ControlType.Document"
            | "ControlType.Text"
            | "ControlType.Window"
    );
    if interesting && !(name.is_empty() && automation_id.is_empty()) {
        let short = control.replace("ControlType.", "");
        let trimmed: String = name.chars().take(80).collect();
        let mut line = format!("- {short}");
        if !trimmed.is_empty() {
            line.push_str(&format!(" \"{trimmed}\""));
        }
        if !automation_id.is_empty() {
            line.push_str(&format!(" (id: {automation_id})"));
        }
        if !enabled {
            line.push_str(" [disabled]");
        }
        if let Some(bounds) = bounds {
            let x = bounds.get("x").and_then(Value::as_f64).unwrap_or(0.0);
            let y = bounds.get("y").and_then(Value::as_f64).unwrap_or(0.0);
            let width = bounds.get("width").and_then(Value::as_f64).unwrap_or(0.0);
            let height = bounds.get("height").and_then(Value::as_f64).unwrap_or(0.0);
            if width > 0.0 && height > 0.0 {
                line.push_str(&format!(
                    " [screen x={:.0} y={:.0} w={:.0} h={:.0}]",
                    x, y, width, height
                ));
            }
        }
        out.push(line);
    }
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            summarize_native_tree(child, out, depth + 1);
        }
    }
}

fn summarize_browser_elements(result: &Value, out: &mut Vec<String>) {
    if let Some(url) = result.get("url").and_then(Value::as_str) {
        out.push(format!("page: {url}"));
    }
    if let Some(title) = result.get("title").and_then(Value::as_str) {
        if !title.is_empty() {
            out.push(format!("title: {title}"));
        }
    }
    if result.get("loginWall").and_then(Value::as_bool) == Some(true) {
        out.push("signal: loginWall — user must sign in; do not invent page data".into());
    }
    if result.get("captcha").and_then(Value::as_bool) == Some(true) {
        out.push("signal: captcha — park and ask the user to complete verification".into());
    }
    if let Some(elements) = result.get("elements").and_then(Value::as_array) {
        for element in elements.iter().take(40) {
            let reference = element.get("ref").and_then(Value::as_str).unwrap_or("");
            let label: String = element
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            let tag = element.get("tag").and_then(Value::as_str).unwrap_or("");
            out.push(format!("- {tag} {reference} \"{label}\""));
        }
    }
}

fn summarize_browser_read(result: &Value, out: &mut Vec<String>) {
    out.push("page content (browser.read preview):".into());
    if result.get("loginWall").and_then(Value::as_bool) == Some(true) {
        out.push("signal: loginWall".into());
    }
    if result.get("captcha").and_then(Value::as_bool) == Some(true) {
        out.push("signal: captcha".into());
    }
    if let Some(text) = result.get("text").and_then(Value::as_str) {
        let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let snippet: String = collapsed.chars().take(2500).collect();
        if snippet.is_empty() {
            out.push("(no extractable structured text on this page)".into());
        } else {
            out.push(snippet);
        }
    }
    if let Some(prose) = result.get("prose").and_then(Value::as_str) {
        let collapsed = prose.split_whitespace().collect::<Vec<_>>().join(" ");
        let snippet: String = collapsed.chars().take(1500).collect();
        if !snippet.is_empty() {
            out.push(format!("read-prose: {snippet}"));
        }
    }
    if result.get("hasMore").and_then(Value::as_bool) == Some(true) {
        let next = result
            .get("nextOffset")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        out.push(format!(
            "(more content available — browser.read {{\"offset\":{next}}} or browser.scroll)"
        ));
    }
}

/// Browser playbook injected when the goal targets the web. Capability
/// availability is explicit: extension-only methods are never advertised in
/// native visual mode, so the planner cannot route an Edge action into a bridge
/// that is not connected.
fn browser_skill_block(bridge_connected: bool) -> &'static str {
    if !bridge_connected {
        return r#"

BROWSER SKILL — NATIVE VISUAL MODE (the optional extension is NOT connected):
- `browser.*` methods and the pseudo-application "Installed browser" are unavailable. Never propose either one.
- Operate the exact native browser listed in TARGET APPLICATIONS (normally Microsoft Edge).
- Navigate with one bounded action: navigateApplication {"url":"https://..."}. Core restricts it to Edge/Chrome/Brave and HTTP(S). Then read the mark catalog.
- Act by mark id (n12). findElement {"text":"Post"} searches the tree and returns marks. invokeElement/setValue/typeText/click take {"mark":"n12"}.
- Screenshots are set-of-mark annotated. If the control has no badge, probe {"nx":0.42,"ny":0.61} in bitmap space, then act on the returned mark. Never emit screen x,y. Live browser clicks require a mark or a matching page control — nx/ny without one is refused.
- Marks flagged chrome are browser toolbar/address bar — do not type page content into them.
- For X posting, navigating directly to https://x.com/compose/post is allowed. Confirm the signed-in composer is visible. Target its editable mark—not the address bar—when entering text; typeText itself verifies the text landed there. invokeElement the enabled composer Post mark, then navigate to the user's profile or X Latest search and re-observe. The exact authored text must appear as a published, non-editable post before claiming completion; a closed composer or successful click receipt is not publication proof.
- A login wall or CAPTCHA is a concrete blocker. Never invent success and never ask for credentials.
- One small action per reply. Do not retry an unchanged action after an error."#;
    }
    r#"

BROWSER SKILL — DOM ACCELERATOR CONNECTED:
The optional extension is connected, so `browser.*` methods and application "Installed browser" are available for the user's already-logged-in Chromium tab. Every action remains policy-gated.
1. browser.navigate {"url":"..."} when the goal includes a link or you must change pages.
2. browser.wait {"text":"visible fragment","timeoutMs":12000} after navigations while SPAs load.
3. browser.observe lists interactive controls with refs (e1, e2, …). Use it before click/type when you need refs.
4. browser.read {"offset":0} extracts PAGE TEXT (headings, tables, grids, articles, error lists). observe does NOT include article/table body text.
5. Page long content: browser.scroll {"direction":"down"} or {"text":"Error"} then browser.read again; or browser.read {"offset":N} while history says hasMore.
6. browser.find {"text":"RUM"} returns matching refs — then browser.click {"ref":"e3"}. Prefer find when labels are known.
7. browser.getText {"ref"} for a single field; prefer browser.read for analysis of lists/tables.
8. Login wall or CAPTCHA signals: report the concrete blocker. Never invent portal data.
9. Analysis goals (Datadog RUM, logs, dashboards): navigate → wait → read → scroll/read more → summarize ONLY facts present in CURRENT DESKTOP STATE or ACTION HISTORY digests. If text is empty, say so; do not fabricate error rates or stack traces.
10. One small action per reply. Prefer browser.find+click over any coordinate click. After navigate, wait for the destination path — same-origin is not success.
11. browser.hover {"ref"} and browser.dblclick {"ref"} are available for menus and dense grids.
12. Send/publish an already-written draft by clicking the live Send/Post control, then observe the destination (sent folder, profile, confirmation). Do not invent authored text that was already on the page."#
}

fn goal_needs_browser_skill(goal: &str, applications: &[String]) -> bool {
    if applications
        .iter()
        .any(|application| application.eq_ignore_ascii_case("Installed browser"))
    {
        return true;
    }
    let lower = goal.to_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || lower.contains("browser")
        || lower.contains("website")
        || lower.contains("portal")
        || lower.contains("dashboard")
        || lower.contains("datadog")
        || lower.contains("reddit")
        || lower.contains("x.com")
        || lower.contains("tweet")
        || lower.contains("twitter")
        || lower.contains("social media")
        || lower.contains("docs.google")
        || lower.contains("gmail")
        || lower.contains("email")
        || lower.contains("chrome")
        || lower.contains("edge")
}

/// Infers target applications from the goal text when the user did not list
/// any. Aliases cover the launch allow-list plus common apps Alfred can observe
/// once they are running. Word-boundary matching avoids hits like "alleged"
/// containing "edge". An empty result is fine — the planner then picks the
/// applications itself (the prompt tells it how).
fn infer_applications_from_goal(goal: &str) -> Vec<String> {
    const ALIASES: &[(&[&str], &str)] = &[
        (&["notepad++"], "Notepad++"),
        (&["notepad"], "Notepad"),
        (&["calculator", "calc"], "Calculator"),
        (&["paint"], "Paint"),
        (&["file explorer", "explorer"], "File Explorer"),
        (&["microsoft edge", "edge"], "Microsoft Edge"),
        (&["google chrome", "chrome"], "Google Chrome"),
        (&["brave"], "Brave"),
        (
            &[
                "browser",
                "website",
                "web page",
                "webpage",
                "portal",
                "dashboard",
                "site",
                "x.com",
                "tweet",
                "twitter",
                "social media",
                "gmail",
                "docs.google",
                "reddit",
                "datadog",
            ],
            "Microsoft Edge",
        ),
        (&["excel", "spreadsheet", "workbook"], "Microsoft Excel"),
        (&["ms word", "word document", "word"], "Microsoft Word"),
        (&["powerpoint", "presentation"], "Microsoft PowerPoint"),
        (&["outlook", "email", "e-mail", "inbox"], "Microsoft Outlook"),
    ];
    let normalized = goal.to_lowercase();
    let words: std::collections::HashSet<&str> = normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let mut applications: Vec<&str> = Vec::new();
    for (aliases, application) in ALIASES {
        let mentioned = aliases.iter().any(|alias| {
            if alias.chars().any(|character| !character.is_alphanumeric()) {
                normalized.contains(alias)
            } else {
                words.contains(alias)
            }
        });
        if mentioned && !applications.contains(application) {
            applications.push(application);
        }
    }
    applications.into_iter().map(str::to_string).collect()
}

/// TARGET APPLICATIONS line for the planner prompt: the chosen apps, or — when
/// the user left the list empty — instructions to pick them from the goal.
fn planner_app_list(applications: &[String]) -> String {
    if applications.is_empty() {
        "(none chosen — infer them from the goal: use listApplications with application \"Alfred\" to see what is running, then launchApplication for allow-listed apps)".to_string()
    } else {
        applications.join(", ")
    }
}

fn planner_app_rule_for_capabilities(
    applications: &[String],
    bridge_connected: bool,
) -> &'static str {
    if bridge_connected {
        if applications.is_empty() {
            "Name the target application yourself in every action, based on the goal. Use application \"Installed browser\" for browser.* actions."
        } else {
            "Use application names exactly as listed. For browser.* actions, use the listed pseudo-application \"Installed browser\"."
        }
    } else if applications.is_empty() {
        "Name one exact native application in every action based on the goal. The pseudo-application \"Installed browser\" and every browser.* method are unavailable."
    } else {
        "Use application names exactly as listed. The pseudo-application \"Installed browser\" and every browser.* method are unavailable."
    }
}

fn planner_methods(bridge_connected: bool) -> &'static str {
    if bridge_connected {
        "listApplications (application \"Alfred\") | listInstalledApplications (application \"Alfred\") | browser.observe | browser.navigate {\"url\"} | browser.find {\"text\"} | browser.click {\"ref\"} | browser.type {\"ref\",\"text\"} | browser.getText {\"ref\"} | browser.read {\"offset\":0} | browser.scroll {\"direction\":\"down\"} or {\"text\"} | browser.wait {\"text\",\"timeoutMs\"} | browser.hover {\"ref\"} | browser.dblclick {\"ref\"} | launchApplication | focusApplication | activate | observeWindow | findElement {\"text\"} | getValue {\"mark\"} | invokeElement {\"mark\"} | setValue {\"mark\",\"value\"} | typeText {\"mark\",\"text\"} | click {\"mark\"} | scroll {\"mark\"|\"direction\"|\"text\"} | probe {\"nx\",\"ny\"} | rightClick {\"mark\"} | doubleClick {\"mark\"} | hover {\"mark\"} | drag {\"from\",\"to\"} | key | shortcut"
    } else {
        "listApplications (application \"Alfred\") | listInstalledApplications (application \"Alfred\") | launchApplication (exact Start-menu name) | navigateApplication {\"url\":\"https://...\"} (Edge/Chrome/Brave + HTTP(S)) | focusApplication | activate | observeWindow | captureWindow | findElement {\"text\"} or {\"automationId\"|\"name\"|\"controlType\"} | getValue {\"mark\"} | invokeElement {\"mark\"} | setValue {\"mark\",\"value\"} | typeText {\"mark\",\"text\"} | click {\"mark\"} | probe {\"nx\",\"ny\"} | scroll {\"mark\"|\"direction\"|\"text\"} | rightClick {\"mark\"} | doubleClick {\"mark\"} | hover {\"mark\"} | drag {\"from\",\"to\"} | key {\"virtualKey\":13|9|27} | shortcut {\"keys\":\"CTRL+L\"|\"CTRL+S\"}"
    }
}

fn build_planner_prompt(
    goal: &str,
    applications: &[String],
    observations: &str,
    history: &[String],
    plan: &[String],
    bridge_connected: bool,
) -> String {
    let history_text = if history.is_empty() {
        "(none yet)".to_string()
    } else {
        history.join("\n")
    };
    let plan_text = if plan.is_empty() {
        String::new()
    } else {
        let outline = plan
            .iter()
            .enumerate()
            .map(|(index, phase)| format!("{}. {}", index + 1, phase))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\nCURRENT PLAN (your outline; follow it, or return an updated plan):\n{outline}")
    };
    let skill = if goal_needs_browser_skill(goal, applications) {
        browser_skill_block(bridge_connected)
    } else {
        ""
    };
    let capability = if bridge_connected {
        "optional DOM browser accelerator: connected"
    } else {
        "optional DOM browser accelerator: not connected; native application control only"
    };
    let mut prompt = format!(
        "You are the planning brain of Alfred, a supervised desktop automation agent running on the user's machine. Propose the next single action toward the goal.\n\nGOAL: {goal}\n\nTARGET APPLICATIONS: {apps}\n\nAVAILABLE CAPABILITIES: {capability}\n\nCURRENT DESKTOP STATE:\n{observations}\n\nACTION HISTORY (oldest first):\n{history_text}{plan_text}{skill}\n\nReply with exactly one JSON object and nothing else (no markdown fences, no prose):\n{{\"done\": false, \"title\": \"short human label\", \"kind\": \"<method>\", \"application\": \"<exact app name>\", \"intent\": \"what and why\", \"effect\": \"observe|create|modify_reversible|external_write\", \"targetLabel\": \"<element label>\", \"payload\": {{...}}}}\nWhen the goal appears fully complete, reply: {{\"done\": true, \"summary\": \"what was accomplished\"}}. Alfred treats this as a completion claim and performs a fresh evidence-review turn before closing the run.\nIf the goal has multiple phases, you may instead reply {{\"plan\": [\"short phase\", ...]}} (no \"kind\") to outline or revise your approach; it is pinned below as CURRENT PLAN.\n\nMethods available on this turn: {methods}.\n\nRules:\n- One small action per reply; observe or find before click/type when unsure.\n- NEVER propose deletion, trash, purge, overwrite, password entry, shell commands, or credential handling. Alfred hard-blocks persistent data loss regardless of what you return.\n- Prefer marks (n12) or browser refs (e3). findElement {{\"text\"}} / browser.find before acting. Never emit screen x,y; use probe {{nx,ny}} only when a control has no mark.\n- Prefer setValue/invokeElement on a mark (semantic, focus-independent) over click/typeText.\n- Never include processId; Alfred injects the live one.\n- {app_rule}\n- If the last action failed or changed nothing, try a different approach instead of repeating it.\n- If a target application is not running, propose launchApplication. If the exact installed name is uncertain, call listInstalledApplications first.\n- Never claim to have read or analyzed content that does not appear in CURRENT DESKTOP STATE or ACTION HISTORY; if you cannot access the content the goal needs, reply done with a summary explaining the blocker instead of inventing results.",
        apps = planner_app_list(applications),
        methods = planner_methods(bridge_connected),
        app_rule = planner_app_rule_for_capabilities(applications, bridge_connected),
    );
    prompt.push_str(
        "\n- For typeText, name the intended editable control in targetLabel and pass its mark. Alfred rejects the action unless it can focus that target and read the entered text back from it.\n- Never treat a successful input or click call as proof of the final outcome. Re-observe the destination and verify the requested result itself is visible.",
    );
    prompt.push_str(
        "\n- `shortcut` is available for two allow-listed combinations only: {\"keys\":\"CTRL+L\"} focuses a browser/Explorer address bar; {\"keys\":\"CTRL+S\"} opens Save/Save As.",
    );
    prompt
}

fn emit_goal_event(
    app: &AppHandle,
    run_id: &str,
    sequence: usize,
    title: &str,
    detail: &str,
    status: &str,
    progress: u8,
) {
    let _ = app.emit(
        "alfred://run-event",
        RunEvent {
            run_id: run_id.into(),
            sequence,
            step_id: format!("goal-{sequence}"),
            title: title.into(),
            detail: detail.into(),
            application: "Alfred".into(),
            status: status.into(),
            progress,
            evidence_data_url: None,
            timestamp: Utc::now(),
        },
    );
}

/// Builds the textual observation bundle the planner reasons over: one compact
/// section per target application (DOM refs for the browser, UIA control lines
/// for native apps). Returns the bundle and the tab the browser answered from.
fn gather_observations(
    app: &AppHandle,
    run_id: &str,
    applications: &[String],
    pinned_tab: Option<i64>,
) -> (String, Option<i64>) {
    let mut observations = String::new();
    let mut pinned = pinned_tab;
    for application in applications {
        if application == "Installed browser" {
            let mut params = serde_json::json!({});
            if let (Some(tab), Value::Object(ref mut map)) = (pinned, &mut params) {
                map.insert("tabId".to_string(), Value::from(tab));
            }
            match send_browser_command_inner(
                app.clone(),
                BrowserCommand {
                    id: "goal-observe".into(),
                    method: "observe".into(),
                    effect: "observe".into(),
                    intent: "observe the page before planning".into(),
                    target_label: None,
                    params: params.clone(),
                    run_id: Some(run_id.to_string()),
                },
                false,
            ) {
                Ok(value) => {
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    if let Some(tab) = result.get("tabId").and_then(Value::as_i64) {
                        pinned = Some(tab);
                    }
                    let mut lines = vec!["Installed browser:".to_string()];
                    summarize_browser_elements(&result, &mut lines);
                    // Auto page-text preview so analysis goals (portals, RUM,
                    // dashboards) see content without relying on the planner to
                    // remember browser.read on the first turn.
                    let mut read_params = params;
                    if let (Some(tab), Value::Object(ref mut map)) = (pinned, &mut read_params) {
                        map.insert("tabId".to_string(), Value::from(tab));
                    }
                    if let Value::Object(ref mut map) = read_params {
                        map.insert("offset".into(), Value::from(0));
                    }
                    match send_browser_command_inner(
                        app.clone(),
                        BrowserCommand {
                            id: "goal-read".into(),
                            method: "read".into(),
                            effect: "observe".into(),
                            intent: "read page content before planning".into(),
                            target_label: None,
                            params: read_params,
                            run_id: Some(run_id.to_string()),
                        },
                        false,
                    ) {
                        Ok(read_value) => {
                            let read_result =
                                read_value.get("result").cloned().unwrap_or(Value::Null);
                            if let Some(tab) = read_result.get("tabId").and_then(Value::as_i64) {
                                pinned = Some(tab);
                            }
                            lines.push(String::new());
                            summarize_browser_read(&read_result, &mut lines);
                        }
                        Err(error) => {
                            lines.push(format!("page content: unavailable ({error})"));
                        }
                    }
                    observations.push_str(&lines.join("\n"));
                    observations.push('\n');
                }
                Err(error) => {
                    observations.push_str(&format!("Installed browser: unavailable ({error})\n"));
                }
            }
        } else if cfg!(windows) {
            let runtime = app.state::<RuntimeState>();
            let section =
                resolve_application_process_id(app, &runtime, application).and_then(|pid| {
                    let request = ActionRequest {
                        protocol_version: protocol_version(),
                        run_id: run_id.into(),
                        workflow_step: "goal-observe".into(),
                        application: application.clone(),
                        intent: format!("observe {application} before planning"),
                        effect: "observe".into(),
                        target_label: None,
                        payload: Some(serde_json::json!({ "processId": pid })),
                    };
                    execute_native_action_inner(
                        app,
                        &runtime,
                        request,
                        "observeWindow".into(),
                        Duration::from_secs(15),
                        false,
                    )
                });
            match section {
                Ok(tree) => {
                    let mut lines = Vec::new();
                    summarize_native_observation(&tree, application, &mut lines);
                    observations.push_str(&lines.join("\n"));
                    observations.push('\n');
                }
                Err(error) => {
                    observations.push_str(&format!("{application}: unavailable ({error})\n"));
                }
            }
        }
    }
    if observations.trim().is_empty() {
        observations = "(no observations available)".into();
    }
    (observations, pinned)
}

/// Ends a goal run with a failure: event + checkpoint.
fn fail_goal_run(
    app: &AppHandle,
    run_id: &str,
    goal: &str,
    step: usize,
    progress: u8,
    error: String,
) {
    emit_goal_event(
        app,
        run_id,
        step,
        "Goal run failed",
        &error,
        "failed",
        progress,
    );
    let _ = save_checkpoint(
        app,
        &RunCheckpoint {
            run_id: run_id.into(),
            workflow_id: goal.into(),
            next_step_index: step,
            status: "failed".into(),
            error: Some(error),
            updated_at: Utc::now(),
        },
    );
}

const GOAL_RUN_MAX_CONSECUTIVE_FAILURES: u32 = 3;
const MAX_PLANNER_HISTORY: usize = 40;

/// Goal runs have no arbitrary action ceiling. Progress is deliberately
/// asymptotic and stays below 100 until evidence-backed completion.
fn goal_run_progress(step_index: usize) -> u8 {
    ((step_index.saturating_mul(4)).min(95)) as u8
}

fn remember_goal_event(memory: &mut GoalRunMemory, entry: String) {
    memory.history.push(entry);
    while memory.history.len() > MAX_PLANNER_HISTORY {
        memory.history.remove(0);
    }
}

fn goal_requires_published_text(goal: &str) -> bool {
    let lower = goal.to_ascii_lowercase();
    lower.contains("tweet")
        || lower.contains("twitter")
        || (lower.contains("x.com") && lower.contains("post"))
        || lower.contains("publish a post")
        || lower.contains("post on social")
        || lower.contains("post on x")
        || lower.contains("send an email")
        || lower.contains("send email")
        || lower.contains("send the email")
        || lower.contains("publish to")
}

/// Publication proof is only forced when Alfred actually captured authored text
/// this run. "Send the already-written draft" must not loop forever waiting for
/// an absent string. A publish click without typed text still needs ordinary
/// destination-page evidence via the generic completion review.
fn requires_published_text_proof(
    goal: &str,
    last_typed_text: Option<&str>,
    saw_publish_commit: bool,
) -> bool {
    last_typed_text.is_some() && (goal_requires_published_text(goal) || saw_publish_commit)
}

fn authored_text_anchor(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.len() < 3 || trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return None;
    }
    // Save-As paths and bare filenames are not publication text.
    // Do not treat a colon in prose ("Subject: hello") as a path.
    let looks_like_path = trimmed.contains('\\')
        || trimmed.contains('/')
        || (trimmed.len() >= 3
            && trimmed.as_bytes()[1] == b':'
            && trimmed.as_bytes()[0].is_ascii_alphabetic());
    let looks_like_filename = !trimmed.contains(' ')
        && std::path::Path::new(trimmed).extension().is_some_and(|ext| {
            matches!(
                ext.to_string_lossy().to_ascii_lowercase().as_str(),
                "txt" | "md" | "docx" | "xlsx" | "pptx" | "pdf" | "csv" | "rtf"
            )
        });
    if looks_like_path || looks_like_filename {
        return None;
    }
    Some(trimmed.chars().take(500).collect())
}

fn append_completion_review(
    prompt: &mut String,
    claim: &str,
    required_text: Option<&str>,
    require_published_text: bool,
    publish_without_anchor: bool,
) {
    prompt.push_str(&format!(
        "\n\nCOMPLETION REVIEW — do not trust the earlier claim. Re-check it only against CURRENT DESKTOP STATE from this fresh turn. ACTION HISTORY proves only that an input was attempted; it is never final-outcome evidence. Earlier claim: {claim}\n"
    ));
    if require_published_text {
        if let Some(text) = required_text {
            prompt.push_str(&format!(
                "REQUIRED PUBLISHED TEXT: {text}\nThe required text must be visible now as non-editable published content. Text still inside an Edit/Document composer, a closed composer, navigation to a feed, or a successful Post click is not proof. Navigate to the user's profile or an exact Latest search and observe the matching post before completing.\n"
            ));
        }
    } else if publish_without_anchor {
        prompt.push_str(
            "A publish/send control was clicked, but no authored-text anchor was recorded this run. Do not invent one. Complete only if CURRENT DESKTOP STATE shows the destination outcome (sent-mail folder, published post, confirmation page URL/title, or a closed composer plus destination body) — not a successful click receipt.\n",
        );
    }
    prompt.push_str(
        "If every requested outcome is visibly evidenced, reply exactly {\"done\":true,\"verified\":true,\"summary\":\"verified outcome\",\"evidence\":[\"specific text or state visible in CURRENT DESKTOP STATE\"]}. Every evidence item must describe current visible state, not an earlier action receipt. If anything is missing or uncertain, reply with verified:false and the next corrective action using the normal action schema. Never mark a blocker as successful completion.",
    );
}

fn evidence_matches_grounding(evidence: &str, grounding: &str) -> bool {
    const STOP_WORDS: &[&str] = &[
        "this",
        "that",
        "with",
        "from",
        "shows",
        "visible",
        "successful",
        "result",
        "completed",
        "application",
    ];
    let grounding = grounding.to_ascii_lowercase();
    let mut matched = 0;
    for token in evidence
        .to_ascii_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.len() >= 4 && !STOP_WORDS.contains(token))
    {
        if grounding.contains(token) {
            matched += 1;
            if matched >= 2 {
                return true;
            }
        }
    }
    false
}

fn line_looks_like_composer(line: &str) -> bool {
    let trimmed = line.trim_start();
    let role_is_composer = trimmed.contains(" Document")
        || trimmed.contains(" Edit")
        || trimmed.starts_with("- Document ")
        || trimmed.starts_with("- Edit ");
    (role_is_composer && !trimmed.contains(" Hyperlink ") && !trimmed.contains(" ListItem "))
        || trimmed.contains(" chrome")
}

fn line_looks_like_published_evidence(line: &str) -> bool {
    let trimmed = line.trim_start();
    if trimmed.starts_with("page content") {
        return false;
    }
    trimmed.starts_with("text: ")
        || trimmed.starts_with("read-prose:")
        || trimmed.starts_with("- Text ")
        || trimmed.starts_with("- a ")
        || trimmed.starts_with("- Hyperlink ")
        || trimmed.starts_with("- ListItem ")
        || trimmed.contains(" Hyperlink ")
        || trimmed.contains(" ListItem ")
        || trimmed.contains(" Text ")
        || trimmed.starts_with("TABLE:")
        || trimmed.starts_with("LIST:")
        || trimmed.starts_with("GRID:")
        || (trimmed.starts_with('n')
            && (trimmed.contains(" Hyperlink ")
                || trimmed.contains(" ListItem ")
                || trimmed.contains(" Text ")))
}

/// Publication text must be present in static page content. UIA exposes browser
/// composers and address bars as Edit/Document controls, so excluding those
/// prevents a draft (or a misdirected address-bar write) from proving success.
fn line_contains_authored_prefix(line: &str, expected_prefix: &[String]) -> bool {
    let words: Vec<String> = line
        .to_ascii_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    words
        .windows(expected_prefix.len())
        .any(|window| window == expected_prefix)
}

fn observation_contains_published_text(observation: &str, expected: &str) -> bool {
    let expected_words: Vec<String> = expected
        .to_ascii_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    if expected_words.len() < 3 {
        return false;
    }
    let prefix_len = expected_words.len().min(10);
    let expected_prefix = &expected_words[..prefix_len];
    // A still-open composer that holds the authored text means this is the
    // compose surface, not a published post — even if a sibling text: snippet
    // or leftover read-prose repeats the same prefix.
    if observation.lines().any(|line| {
        let trimmed = line.trim_start();
        line_looks_like_composer(trimmed) && line_contains_authored_prefix(trimmed, expected_prefix)
    }) {
        return false;
    }
    observation.lines().any(|line| {
        let trimmed = line.trim_start();
        if line_looks_like_composer(trimmed) {
            return false;
        }
        if !line_looks_like_published_evidence(trimmed) {
            return false;
        }
        // UIA labels are intentionally capped at 80 characters, so allow the
        // tail to be truncated while requiring the authored word sequence—not
        // just a bag of common topic words—in one static element. Aggregating
        // across unrelated search results would allow a similar post to pass.
        line_contains_authored_prefix(trimmed, expected_prefix)
    })
}

fn is_verified_completion(
    reply: &PlannerReply,
    current_observation: &str,
    required_text: Option<&str>,
    require_published_text: bool,
) -> bool {
    reply.done
        && reply.verified == Some(true)
        && reply.evidence.as_ref().is_some_and(|items| {
            !items.is_empty()
                && items
                    .iter()
                    .all(|item| evidence_matches_grounding(item, current_observation))
        })
        && (!require_published_text
            || required_text
                .is_some_and(|text| observation_contains_published_text(current_observation, text)))
}

fn goal_requires_save_proof(goal: &str) -> bool {
    let lower = goal.to_ascii_lowercase();
    !goal_requires_published_text(goal)
        && !lower.contains("save me")
        && (lower.contains("save as")
            || lower.contains("save the file")
            || lower.contains("save this file")
            || lower.contains("save the document")
            || lower.contains("save this document")
            || lower.contains("save to desktop")
            || lower.contains("save it as")
            || lower.contains("save it to"))
}

fn live_resolved_label(value: &Value) -> Option<String> {
    let result = value.get("result").unwrap_or(value);
    result
        .get("targetName")
        .or_else(|| result.get("label"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| label.chars().take(160).collect())
}

fn is_publish_commit_label(label: &str) -> bool {
    let lowered = label.to_ascii_lowercase();
    let tokens: Vec<&str> = lowered
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let Some(first) = tokens.first() else {
        return false;
    };
    let publish_verb = matches!(*first, "post" | "publish" | "tweet")
        || (*first == "send"
            && tokens
                .iter()
                .any(|token| matches!(*token, "tweet" | "post" | "email" | "message")));
    publish_verb
        && !tokens.iter().any(|token| {
            matches!(
                *token,
                "text"
                    | "to"
                    | "screen"
                    | "link"
                    | "sheet"
                    | "invite"
                    | "invitation"
                    | "file"
                    | "print"
                    | "meeting"
                    | "calendar"
            )
        })
}

fn is_save_commit(kind: &str, label: Option<&str>, payload: Option<&Value>) -> bool {
    if kind == "shortcut" {
        return payload
            .and_then(|value| value.get("keys"))
            .and_then(Value::as_str)
            .is_some_and(|keys| keys.eq_ignore_ascii_case("CTRL+S"));
    }
    label.is_some_and(|value| {
        let lowered = value.to_ascii_lowercase();
        if lowered.contains("don't")
            || lowered.contains("dont")
            || lowered.contains("do not")
        {
            return false;
        }
        let tokens: Vec<&str> = lowered
            .split(|character: char| !character.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .collect();
        tokens.iter().any(|token| *token == "save")
            && !tokens
                .iter()
                .any(|token| matches!(*token, "as" | "not" | "never" | "cancel" | "dont"))
    })
}

fn observation_title_for(observation: &str, application: Option<&str>) -> String {
    if let Some(app) = application.map(str::trim).filter(|name| !name.is_empty()) {
        let app_lower = app.to_ascii_lowercase();
        let mut in_section = false;
        for line in observation.lines() {
            let trimmed = line.trim_start();
            let lower = trimmed.to_ascii_lowercase();
            let app_header = lower.starts_with(&app_lower)
                && lower
                    .as_bytes()
                    .get(app_lower.len())
                    .is_none_or(|byte| matches!(*byte, b' ' | b':' | b'\t'));
            if app_header {
                in_section = true;
                continue;
            }
            if in_section {
                if let Some(title) = trimmed.strip_prefix("title:") {
                    return title.trim().to_string();
                }
                let next_app = trimmed.contains("  gen=")
                    || (trimmed.ends_with(':')
                        && !trimmed.starts_with("title:")
                        && !trimmed.starts_with("text:")
                        && !trimmed.starts_with("page:")
                        && !trimmed.starts_with("read-prose:"));
                if next_app {
                    break;
                }
            }
        }
    }
    observation
        .lines()
        .find_map(|line| {
            line.trim_start()
                .strip_prefix("title:")
                .map(|title| title.trim().to_string())
        })
        .unwrap_or_default()
}

fn looks_like_filename(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    [".txt", ".docx", ".xlsx", ".pdf", ".png", ".md", ".csv"]
        .iter()
        .any(|ext| lower.contains(ext))
}

fn observation_shows_save_transition(
    current: &str,
    baseline: Option<&str>,
    expected_name: Option<&str>,
    save_committed: bool,
    application: Option<&str>,
) -> bool {
    if !save_committed {
        return false;
    }
    let Some(baseline) = baseline else {
        return false;
    };
    let current_title = observation_title_for(current, application);
    let old_title = observation_title_for(baseline, application);
    if let Some(name) = expected_name {
        let file = name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(name)
            .to_ascii_lowercase();
        // Filename proof is title-only. An Edit in a still-open Save As dialog
        // often exposes the typed name as its accessible name; that is not a save.
        if file.len() >= 4
            && current_title.to_ascii_lowercase().contains(&file)
            && !old_title.to_ascii_lowercase().contains(&file)
        {
            return true;
        }
    }
    let old_dirty = old_title.contains('*');
    let now_clean = !current_title.contains('*') && !current_title.is_empty();
    if old_dirty
        && now_clean
        && current_title.replace('*', "").trim() == old_title.replace('*', "").trim()
    {
        return true;
    }
    looks_like_filename(&current_title) && current_title != old_title
}

/// The agent loop: observe → plan → policy-gate → execute → record, until the
/// planner declares the goal done, a guardrail trips, or the user stops the run.
/// Every action flows through the same policy engine, approval parking, run lock,
/// and targeted executors as recorded workflows — the planner only proposes.
async fn drive_goal_run(
    app: AppHandle,
    mut memory: GoalRunMemory,
    check_in_every: u32,
    share_screenshots: bool,
) {
    let run_id = memory.run_id.clone();
    let provider = memory.provider.clone();
    let goal = memory.goal.clone();
    let lock_path = run_lock_path(&app).ok();
    loop {
        let step_index = memory.next_step_index;
        // This is a durable planner-turn sequence, not a completion budget.
        // Advancing before the turn also makes crash recovery monotonic.
        memory.next_step_index = memory.next_step_index.saturating_add(1);
        let bridge_connected = browser_bridge_status(app.clone()).unwrap_or(false);
        if bridge_connected && goal_needs_browser_skill(&goal, &memory.applications) {
            if !memory
                .applications
                .iter()
                .any(|application| application.eq_ignore_ascii_case("Installed browser"))
            {
                memory.applications.push("Installed browser".into());
            }
        } else if !bridge_connected {
            // A saved/continued run may contain the bridge pseudo-application
            // from an earlier turn. Fall back to an executable native target as
            // soon as the optional accelerator disappears.
            for application in &mut memory.applications {
                if application.eq_ignore_ascii_case("Installed browser") {
                    *application = "Microsoft Edge".into();
                }
            }
            memory.applications.sort_by_key(|item| item.to_lowercase());
            memory
                .applications
                .dedup_by(|left, right| left.eq_ignore_ascii_case(right));
            memory.pinned_browser_tab = None;
        }
        if wait_if_paused(&app, &run_id).await {
            memory.status = "stopped".into();
            let _ = save_goal_run_memory(&app, &mut memory);
            stop_run(&app, &run_id, &goal, step_index as usize);
            return;
        }
        if let Some(path) = &lock_path {
            write_run_lock(path, &run_id);
        }
        // Mid-run user guidance from the cockpit's steer bar joins the history
        // the planner reasons over on this turn.
        if let Ok(mut notes) = app.state::<RuntimeState>().steer_notes.lock() {
            if let Some(mut queued) = notes.remove(&run_id) {
                for note in queued.drain(..) {
                    remember_goal_event(&mut memory, format!("user guidance: {note}"));
                }
            }
        }
        let _ = save_goal_run_memory(&app, &mut memory);
        let progress = goal_run_progress(step_index);
        emit_goal_event(
            &app,
            &run_id,
            step_index as usize,
            "Observing the desktop",
            "Reading the current state of every target application.",
            "running",
            progress,
        );
        let (observations, new_pinned) = gather_observations(
            &app,
            &run_id,
            &memory.applications,
            memory.pinned_browser_tab,
        );
        memory.pinned_browser_tab = new_pinned;
        memory.last_observation = compact_planner_observations(&observations);
        let _ = save_goal_run_memory(&app, &mut memory);
        // Visual grounding: one screenshot per target app. Attached to the planner
        // turn only for CLIs with verified image input (Codex); also shown in the
        // cockpit timeline as evidence.
        let (shot_paths, shot_evidence) = if share_screenshots {
            capture_run_screenshots(
                &app,
                &run_id,
                &memory.applications,
                step_index.min(u32::MAX as usize) as u32,
                memory.pinned_browser_tab,
            )
        } else {
            (Vec::new(), None)
        };
        emit_goal_event(
            &app,
            &run_id,
            step_index as usize,
            "Planning the next action",
            &format!("{provider} is deciding the next step."),
            "running",
            progress,
        );
        let mut prompt = build_planner_prompt(
            &goal,
            &memory.applications,
            &memory.last_observation,
            &memory.history,
            &memory.working_plan,
            bridge_connected,
        );
        if let Some(claim) = memory.completion_claim.as_deref() {
            append_completion_review(
                &mut prompt,
                claim,
                memory.last_typed_text.as_deref(),
                requires_published_text_proof(
                    &goal,
                    memory.last_typed_text.as_deref(),
                    memory.saw_publish_commit,
                ),
                memory.saw_publish_commit && memory.last_typed_text.is_none(),
            );
        }
        // Flag providers receive the files as CLI attachments; path providers get
        // the file list in the prompt and their file reader supplies the vision.
        let delivery = provider_image_delivery(&provider);
        let flag_images: &[PathBuf] = match delivery {
            Some(ImageDelivery::Flag) => &shot_paths,
            _ => &[],
        };
        if !shot_paths.is_empty() {
            match delivery {
                Some(ImageDelivery::Flag) => prompt.push_str(&format!(
                    "\n\nSCREENSHOTS ATTACHED: {} image(s), one per target application in the order listed. Cross-check them against the text observations; if they disagree, trust the screenshot.",
                    shot_paths.len()
                )),
                Some(ImageDelivery::PromptPaths) => {
                    prompt.push_str("\n\nSCREENSHOT FILES — use your file-reading tool to view each image below; they show the current desktop state, one per target application in the order listed. Cross-check them against the text observations; if they disagree, trust the screenshot:");
                    for path in &shot_paths {
                        prompt.push_str(&format!("\n- {}", path.display()));
                    }
                }
                None => {}
            }
        }
        // Grok/Cursor argv length: spill oversized prompts to a temp file.
        let (prompt_for_cli, spilled_prompt) =
            match materialize_planner_prompt(&app, &provider, &prompt) {
                Ok(pair) => pair,
                Err(error) => {
                    fail_goal_run(
                        &app,
                        &run_id,
                        &goal,
                        step_index as usize,
                        progress,
                        format!("Could not prepare the planner prompt: {error}"),
                    );
                    return;
                }
            };
        let resume_provider_session =
            memory.planner_turns > 0 && memory.provider_session_id.is_some();
        let output = match run_planner_turn(
            &app,
            &run_id,
            &provider,
            &prompt_for_cli,
            flag_images,
            memory.provider_session_id.as_deref(),
            resume_provider_session,
            step_index as usize,
            progress,
        )
        .await
        {
            Ok((output, emitted_session_id)) => {
                if let Some(path) = spilled_prompt.as_ref() {
                    let _ = fs::remove_file(path);
                }
                if memory.provider_session_id.is_none() {
                    memory.provider_session_id = emitted_session_id;
                }
                memory.planner_turns += 1;
                let _ = save_goal_run_memory(&app, &mut memory);
                output
            }
            Err(error) if error == "stopped" => {
                if let Some(path) = spilled_prompt.as_ref() {
                    let _ = fs::remove_file(path);
                }
                stop_run(&app, &run_id, &goal, step_index as usize);
                return;
            }
            Err(error) => {
                if let Some(path) = spilled_prompt.as_ref() {
                    let _ = fs::remove_file(path);
                }
                if resume_provider_session && memory.provider_session_resets < 1 {
                    memory.provider_session_resets += 1;
                    memory.planner_turns = 0;
                    memory.provider_session_id = if matches!(provider.as_str(), "grok" | "copilot")
                    {
                        Some(Uuid::new_v4().to_string())
                    } else {
                        None
                    };
                    remember_goal_event(
                        &mut memory,
                        format!("provider session could not resume; rebuilt from Alfred memory: {error}"),
                    );
                    let _ = save_goal_run_memory(&app, &mut memory);
                    emit_goal_event(
                        &app,
                        &run_id,
                        step_index as usize,
                        "Rebuilding planner context",
                        "The provider session was unavailable. Alfred preserved the run and is starting a replacement session from durable memory.",
                        "running",
                        progress,
                    );
                    continue;
                }
                memory.consecutive_failures += 1;
                remember_goal_event(&mut memory, format!("planner error: {error}"));
                let _ = save_goal_run_memory(&app, &mut memory);
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    "Planner error",
                    &error,
                    "running",
                    progress,
                );
                if memory.consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                    memory.status = "failed".into();
                    let _ = save_goal_run_memory(&app, &mut memory);
                    fail_goal_run(
                        &app,
                        &run_id,
                        &goal,
                        step_index as usize,
                        progress,
                        format!("The planner is unreachable: {error}"),
                    );
                    return;
                }
                continue;
            }
        };
        let reply = match parse_planner_action(&output) {
            Ok(reply) => reply,
            Err(error) => {
                memory.consecutive_failures += 1;
                // Show the planner what its unusable output looked like so the
                // next turn can fix the format instead of repeating it — and
                // surface a short snippet in the cockpit so the user can see why.
                let snippet: String = output.trim().chars().take(400).collect();
                remember_goal_event(&mut memory, format!("{error} Output began: {snippet}"));
                let _ = save_goal_run_memory(&app, &mut memory);
                let detail = if snippet.is_empty() {
                    format!("{error} (empty planner output — often a CLI/argv or auth failure)")
                } else {
                    format!("{error} Snippet: {snippet}")
                };
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    "Unusable planner output",
                    &detail.chars().take(500).collect::<String>(),
                    "running",
                    progress,
                );
                if memory.consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                    memory.status = "failed".into();
                    let _ = save_goal_run_memory(&app, &mut memory);
                    fail_goal_run(
                        &app,
                        &run_id,
                        &goal,
                        step_index as usize,
                        progress,
                        format!(
                            "The planner kept returning unusable output. Last snippet: {}",
                            snippet.chars().take(200).collect::<String>()
                        ),
                    );
                    return;
                }
                continue;
            }
        };
        if reply.done {
            let require_published_text = requires_published_text_proof(
                &goal,
                memory.last_typed_text.as_deref(),
                memory.saw_publish_commit,
            );
            let verified_reply = is_verified_completion(
                &reply,
                &memory.last_observation,
                memory.last_typed_text.as_deref(),
                require_published_text,
            ) && (!goal_requires_save_proof(&goal)
                || observation_shows_save_transition(
                    &memory.last_observation,
                    memory.save_baseline.as_deref(),
                    memory.last_typed_text.as_deref(),
                    memory.save_committed,
                    memory.save_application.as_deref(),
                ));
            let summary = reply
                .summary
                .unwrap_or_else(|| "The planner reports the goal is complete.".into());
            if memory.completion_claim.is_none() {
                memory.completion_claim = Some(summary.clone());
                memory.verification_attempts = 0;
                remember_goal_event(
                    &mut memory,
                    format!("completion claim awaiting evidence review: {summary}"),
                );
                let _ = save_goal_run_memory(&app, &mut memory);
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    "Verifying the outcome",
                    "The planner's completion claim is not final. Alfred is taking a fresh observation and checking concrete evidence.",
                    "running",
                    progress,
                );
                continue;
            }
            let evidence = reply.evidence.unwrap_or_default();
            if verified_reply {
                memory.status = "completed".into();
                memory.completion_summary = Some(summary.clone());
                memory.completion_evidence = evidence.clone();
                memory.pending_action = None;
                memory.next_step_index = step_index.saturating_add(1);
                let _ = save_goal_run_memory(&app, &mut memory);
                let _ = save_checkpoint(
                    &app,
                    &RunCheckpoint {
                        run_id: run_id.clone(),
                        workflow_id: goal.clone(),
                        next_step_index: step_index.saturating_add(1),
                        status: "completed".into(),
                        error: None,
                        updated_at: Utc::now(),
                    },
                );
                let detail = format!("{summary} Evidence: {}", evidence.join(" · "));
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    "Goal verified and completed",
                    &detail,
                    "completed",
                    100,
                );
                return;
            }
            memory.verification_attempts += 1;
            memory.completion_claim = None;
            memory.completion_evidence.clear();
            remember_goal_event(
                &mut memory,
                format!("completion review rejected the claim: {summary}"),
            );
            let _ = save_goal_run_memory(&app, &mut memory);
            emit_goal_event(
                &app,
                &run_id,
                step_index as usize,
                "Completion not yet verified",
                "The review found no concrete completion evidence. Planning will continue.",
                "running",
                progress,
            );
            continue;
        }
        if memory.completion_claim.take().is_some() {
            remember_goal_event(
                &mut memory,
                "completion review found a gap; applying the proposed corrective action".into(),
            );
        }
        // Multi-phase goals: the planner may outline or revise its approach
        // instead of acting. The outline is pinned into every later prompt and
        // shown in the cockpit timeline.
        if !reply.done && reply.kind.is_none() {
            if let Some(plan) = reply.plan.clone().filter(|plan| !plan.is_empty()) {
                memory.working_plan = plan;
                memory.consecutive_failures = 0;
                let outline = memory.working_plan.join(" → ");
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    "Plan outlined",
                    &outline,
                    "running",
                    progress,
                );
                remember_goal_event(&mut memory, format!("plan: {outline}"));
                let _ = save_goal_run_memory(&app, &mut memory);
                continue;
            }
        }
        // 3. Execute through the same policy-gated path as recorded workflows.
        let mut kind = reply.kind.clone().unwrap_or_default();
        let mut application = reply.application.clone().unwrap_or_else(|| {
            if kind.starts_with("browser.") {
                "Installed browser".into()
            } else {
                memory
                    .applications
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Alfred".into())
            }
        });
        // Compatibility fallback for a provider that ignores the native method
        // list. Navigation has an equivalent bounded host capability; all other
        // bridge actions remain rejected and are sent back for re-planning.
        if kind == "browser.navigate" && !bridge_connected {
            kind = "navigateApplication".into();
            application = native_browser_application(&memory.applications);
            remember_goal_event(
                &mut memory,
                "translated unavailable browser.navigate to safe native navigateApplication".into(),
            );
        }
        let is_browser = kind.starts_with("browser.");
        if is_browser && !bridge_connected {
            let message = format!(
                "Rejected unavailable action {kind}: the optional browser extension is not connected. Use the native browser methods listed in AVAILABLE CAPABILITIES."
            );
            remember_goal_event(&mut memory, message.clone());
            memory.pending_action = None;
            memory.consecutive_failures += 1;
            let _ = save_goal_run_memory(&app, &mut memory);
            emit_goal_event(
                &app,
                &run_id,
                step_index as usize,
                "Planner chose an unavailable browser action",
                &message,
                "running",
                progress,
            );
            if memory.consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                memory.status = "failed".into();
                let _ = save_goal_run_memory(&app, &mut memory);
                fail_goal_run(
                    &app,
                    &run_id,
                    &goal,
                    step_index as usize,
                    progress,
                    "The planner repeatedly ignored the available native browser capabilities."
                        .into(),
                );
                return;
            }
            continue;
        }
        if application != "Alfred"
            && !memory
                .applications
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&application))
        {
            memory.applications.push(application.clone());
        }
        let title = reply.title.clone().unwrap_or_else(|| kind.clone());
        let declared_effect = reply.effect.clone().unwrap_or_else(|| "unknown".into());
        let step = WorkflowStep {
            id: format!("goal-step-{step_index}"),
            title: title.clone(),
            kind: kind.clone(),
            effect: effective_effect_for(
                &kind,
                &declared_effect,
                reply.target_label.as_deref(),
                reply.payload.as_ref(),
            ),
            application: Some(application.clone()),
            intent: reply.intent.clone(),
            target_label: reply.target_label.clone(),
            payload: reply.payload.clone(),
            timeout_ms: 30_000,
            retries: 1,
            wait_for: None,
            expect: None,
            save_as: None,
        };
        if !is_browser && kind == "click" {
            let payload = step.payload.as_ref();
            let has_mark = payload
                .and_then(|value| value.get("mark"))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty());
            let has_normalized = payload_has_normalized_point(payload);
            if !has_mark && !has_normalized {
                let message = "Rejected invalid planner action click: live goals must click a mark from observe/find/probe, not raw screen coordinates.".to_string();
                remember_goal_event(&mut memory, message.clone());
                memory.pending_action = None;
                memory.consecutive_failures += 1;
                let _ = save_goal_run_memory(&app, &mut memory);
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    "Planner proposed an invalid action",
                    &message,
                    "running",
                    progress,
                );
                if memory.consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                    memory.status = "failed".into();
                    let _ = save_goal_run_memory(&app, &mut memory);
                    fail_goal_run(
                        &app,
                        &run_id,
                        &goal,
                        step_index as usize,
                        progress,
                        "The planner repeatedly proposed invalid actions.".into(),
                    );
                    return;
                }
                continue;
            }
        }
        if let Err(error) = validate_workflow_step(&step) {
            let message = format!("Rejected invalid planner action {kind}: {error}");
            remember_goal_event(&mut memory, message.clone());
            memory.pending_action = None;
            memory.consecutive_failures += 1;
            let _ = save_goal_run_memory(&app, &mut memory);
            emit_goal_event(
                &app,
                &run_id,
                step_index as usize,
                "Planner proposed an invalid action",
                &message,
                "running",
                progress,
            );
            if memory.consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                memory.status = "failed".into();
                let _ = save_goal_run_memory(&app, &mut memory);
                fail_goal_run(
                    &app,
                    &run_id,
                    &goal,
                    step_index as usize,
                    progress,
                    "The planner repeatedly proposed invalid actions.".into(),
                );
                return;
            }
            continue;
        }
        let mut payload = step
            .payload
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        if !is_browser && cfg!(windows) && needs_process_resolution(&kind, &application) {
            let runtime = app.state::<RuntimeState>();
            match resolve_application_process_id(&app, &runtime, &application) {
                Ok(pid) => {
                    if let Value::Object(ref mut map) = payload {
                        map.insert("processId".into(), Value::from(pid));
                    }
                }
                Err(error) => {
                    memory.consecutive_failures += 1;
                    remember_goal_event(
                        &mut memory,
                        format!("{title} — target unavailable: {error}"),
                    );
                    let _ = save_goal_run_memory(&app, &mut memory);
                    if memory.consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                        memory.status = "failed".into();
                        let _ = save_goal_run_memory(&app, &mut memory);
                        fail_goal_run(
                            &app,
                            &run_id,
                            &goal,
                            step_index as usize,
                            progress,
                            format!("Target application never became available: {error}"),
                        );
                        return;
                    }
                    continue;
                }
            }
        }
        if is_browser {
            if let (Some(tab), Value::Object(map)) = (memory.pinned_browser_tab, &mut payload) {
                map.entry("tabId".to_string()).or_insert(Value::from(tab));
            }
        }
        memory.pending_action = Some(step.clone());
        let _ = save_goal_run_memory(&app, &mut memory);
        emit_goal_event(
            &app,
            &run_id,
            step_index as usize,
            &title,
            &format!("{kind} in {application}, checked by the safety engine."),
            "running",
            progress,
        );
        // A request_user parks the run; on approval the same action re-enters the
        // policy gate (now authorized) instead of being skipped.
        let result = loop {
            let approved = app
                .state::<RuntimeState>()
                .approved_overrides
                .lock()
                .ok()
                .map(|overrides| overrides.get(&run_id) == Some(&step.id))
                .unwrap_or(false);
            let request = ActionRequest {
                protocol_version: protocol_version(),
                run_id: run_id.clone(),
                workflow_step: step.id.clone(),
                application: application.clone(),
                intent: step.intent.clone().unwrap_or_else(|| kind.clone()),
                effect: step.effect.clone(),
                target_label: step.target_label.clone(),
                payload: Some(payload.clone()),
            };
            let attempt_result = if is_browser {
                send_browser_command_inner(
                    app.clone(),
                    BrowserCommand {
                        id: step.id.clone(),
                        method: kind.trim_start_matches("browser.").into(),
                        effect: step.effect.clone(),
                        intent: step.intent.clone().unwrap_or_else(|| title.clone()),
                        target_label: step.target_label.clone(),
                        params: payload.clone(),
                        run_id: Some(run_id.clone()),
                    },
                    approved,
                )
            } else {
                let runtime = app.state::<RuntimeState>();
                execute_native_action_inner(
                    &app,
                    &runtime,
                    request,
                    kind.clone(),
                    Duration::from_secs(30),
                    approved,
                )
            };
            match attempt_result {
                Err(error) if error.starts_with("request_user") => {
                    let approved_now = park_run_for_approval(
                        &app,
                        &run_id,
                        &goal,
                        step_index as usize,
                        progress,
                        &step,
                        &application,
                        error,
                    )
                    .await;
                    if !approved_now {
                        return;
                    }
                    continue;
                }
                other => break other,
            }
        };
        match result {
            Ok(value) => {
                append_goal_run_step(&app, &run_id, &step);
                // Read actions (getText/getValue) put what they read into the
                // history — without the digest the planner only learns "ok".
                remember_goal_event(
                    &mut memory,
                    format!("{title} ({kind}) — ok{}", planner_result_digest(&value)),
                );
                memory.pending_action = None;
                memory.consecutive_failures = 0;
                if step.effect != "observe" {
                    memory.actions_since_check_in += 1;
                }
                if matches!(kind.as_str(), "typeText" | "browser.type") {
                    if let Some(text) = payload
                        .get("text")
                        .or_else(|| payload.get("value"))
                        .and_then(Value::as_str)
                        .and_then(authored_text_anchor)
                    {
                        let keep_existing = memory.last_typed_text.as_ref().is_some_and(|old| {
                            old.chars().count() > text.chars().count()
                        });
                        if !keep_existing {
                            memory.last_typed_text = Some(text);
                            memory.last_typed_application = Some(application.clone());
                        }
                    }
                }
                if let Some(live) = live_resolved_label(&value) {
                    memory.last_resolved_label = Some(live.clone());
                    if matches!(
                        kind.as_str(),
                        "invokeElement" | "click" | "doubleClick" | "browser.click" | "browser.dblclick"
                    ) && is_publish_commit_label(&live)
                    {
                        memory.saw_publish_commit = true;
                    }
                }
                let current_live = live_resolved_label(&value);
                if is_save_commit(kind.as_str(), current_live.as_deref(), Some(&payload)) {
                    if memory.save_baseline.is_none() {
                        memory.save_baseline = Some(memory.last_observation.clone());
                        memory.save_application = Some(application.clone());
                    }
                    memory.save_committed = true;
                }
                if is_browser {
                    if let Some(tab) = value
                        .get("result")
                        .and_then(|result| result.get("tabId"))
                        .and_then(Value::as_i64)
                    {
                        memory.pinned_browser_tab = Some(tab);
                    }
                }
                // A one-step approval override is consumed with its action.
                if let Ok(mut overrides) = app.state::<RuntimeState>().approved_overrides.lock() {
                    if overrides.get(&run_id) == Some(&step.id) {
                        overrides.remove(&run_id);
                    }
                }
                let next_progress = goal_run_progress(step_index.saturating_add(1));
                memory.next_step_index = step_index.saturating_add(1);
                let _ = save_goal_run_memory(&app, &mut memory);
                let _ = app.emit(
                    "alfred://run-event",
                    RunEvent {
                        run_id: run_id.clone(),
                        sequence: step_index as usize,
                        step_id: step.id.clone(),
                        title: title.clone(),
                        detail: "Action completed.".into(),
                        application: application.clone(),
                        status: "completed".into(),
                        progress: next_progress,
                        evidence_data_url: shot_evidence.clone(),
                        timestamp: Utc::now(),
                    },
                );
                let _ = save_checkpoint(
                    &app,
                    &RunCheckpoint {
                        run_id: run_id.clone(),
                        workflow_id: goal.clone(),
                        next_step_index: step_index.saturating_add(1),
                        status: "running".into(),
                        error: None,
                        updated_at: Utc::now(),
                    },
                );
                // Human check-in cadence: pause so the cockpit's Resume button
                // lets the user inspect the desktop before the agent continues.
                if check_in_every > 0 && memory.actions_since_check_in >= check_in_every {
                    memory.actions_since_check_in = 0;
                    let _ = save_goal_run_memory(&app, &mut memory);
                    if let Ok(mut controls) = app.state::<RuntimeState>().run_controls.lock() {
                        controls.insert(run_id.clone(), "paused".into());
                    }
                    emit_goal_event(
                        &app,
                        &run_id,
                        step_index as usize,
                        "Check-in pause",
                        &format!(
                            "{check_in_every} actions completed. Review the desktop, then resume."
                        ),
                        "paused",
                        next_progress,
                    );
                }
            }
            Err(error) => {
                memory.pending_action = None;
                memory.consecutive_failures += 1;
                remember_goal_event(&mut memory, format!("{title} ({kind}) — failed: {error}"));
                let _ = save_goal_run_memory(&app, &mut memory);
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    &title,
                    &format!("Action failed; the planner will adjust: {error}"),
                    "running",
                    progress,
                );
                if memory.consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                    memory.status = "failed".into();
                    let _ = save_goal_run_memory(&app, &mut memory);
                    fail_goal_run(
                        &app,
                        &run_id,
                        &goal,
                        step_index as usize,
                        progress,
                        format!(
                            "The planner is not making progress ({GOAL_RUN_MAX_CONSECUTIVE_FAILURES} consecutive failures; last: {error})"
                        ),
                    );
                    return;
                }
            }
        }
    }
}

/// Starts an agent run: the planner proposes, the policy engine disposes.
/// Takes the same machine-wide run lock as workflow replays.
/// Async so the Windows native-host preflight cannot freeze the WebView's main
/// thread — a frozen window right after "Run goal" looks identical to a hang.
#[tauri::command]
async fn start_goal_run(
    app: AppHandle,
    goal: String,
    applications: Vec<String>,
    provider: Option<String>,
    check_in_every: Option<u32>,
) -> Result<String, String> {
    let goal = goal.trim().to_string();
    if goal.is_empty() {
        return Err("Describe the goal first.".into());
    }
    let mut applications: Vec<String> = applications
        .into_iter()
        .map(|application| application.trim().to_string())
        .filter(|application| !application.is_empty())
        .collect();
    if applications.is_empty() {
        // No apps listed: infer them from the goal text. When nothing matches,
        // the list stays empty and the planner picks the applications itself —
        // the prompt tells it how (listApplications / launchApplication).
        applications = infer_applications_from_goal(&goal);
    }
    let settings = get_settings(app.clone())?;
    let provider = provider
        .map(|value| value.trim().to_lowercase())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| settings.provider.clone());
    if !provider_definitions()
        .iter()
        .any(|definition| definition.0 == provider)
    {
        return Err(format!("Unknown planner provider: {provider}"));
    }
    // Fail fast when the planner CLI cannot be supervised on this machine;
    // otherwise the run dies off-screen and the cockpit looks stuck.
    preflight_provider(&provider)?;
    let check_in_every = check_in_every.unwrap_or(0);
    let run_id = Uuid::new_v4().to_string();
    // Grok and Copilot accept a caller-selected UUID. Codex and Cursor emit the
    // session id on their first structured-output turn and Alfred records it.
    let seeded_session_id = matches!(provider.as_str(), "grok" | "copilot").then(|| run_id.clone());
    let mut memory = GoalRunMemory {
        schema_version: 1,
        run_id: run_id.clone(),
        provider: provider.clone(),
        provider_session_id: seeded_session_id,
        goal: goal.clone(),
        applications: applications.clone(),
        planner_turns: 0,
        provider_session_resets: 0,
        next_step_index: 0,
        pinned_browser_tab: None,
        history: Vec::new(),
        working_plan: Vec::new(),
        consecutive_failures: 0,
        actions_since_check_in: 0,
        last_observation: String::new(),
        pending_action: None,
        completion_claim: None,
        completion_evidence: Vec::new(),
        verification_attempts: 0,
        last_typed_text: None,
        last_typed_application: None,
        last_resolved_label: None,
        save_baseline: None,
        save_application: None,
        saw_publish_commit: false,
        save_committed: false,
        status: "running".into(),
        completion_summary: None,
        updated_at: Utc::now(),
    };
    let lock_path = run_lock_path(&app)?;
    {
        let state = app.state::<RuntimeState>();
        let mut controls = state
            .run_controls
            .lock()
            .map_err(|_| "Run control state is unavailable.")?;
        if controls.contains_key(&run_id) {
            return Err("This run is already active.".into());
        }
        controls.insert(run_id.clone(), "running".into());
    }
    let start = (|| {
        try_acquire_run_lock(&lock_path, &run_id)?;
        if cfg!(windows) {
            // Only the automation host itself must exist up front. Target apps
            // deliberately do NOT have to be running here: observations report
            // them as unavailable and the planner can then propose an
            // allow-listed launchApplication — recovering from closed apps is
            // exactly what the agent loop is for. (Recorded-workflow replay
            // keeps the stricter apps-running preflight because it has no
            // planner to recover from a missing app.)
            native_host_executable(&app)?;
        }
        save_checkpoint(
            &app,
            &RunCheckpoint {
                run_id: run_id.clone(),
                workflow_id: goal.clone(),
                next_step_index: 0,
                status: "running".into(),
                error: None,
                updated_at: Utc::now(),
            },
        )?;
        save_goal_run_memory(&app, &mut memory)
    })();
    if let Err(error) = start {
        release_run_lock(&lock_path, &run_id);
        if let Ok(mut controls) = app.state::<RuntimeState>().run_controls.lock() {
            controls.remove(&run_id);
        }
        return Err(error);
    }
    let emitted_run = run_id.clone();
    let app_for_run = app.clone();
    let share_screenshots = settings.share_screenshots_with_planner;
    let screenshot_retention = settings.screenshot_retention.clone();
    tauri::async_runtime::spawn(async move {
        drive_goal_run(
            app_for_run.clone(),
            memory,
            check_in_every,
            share_screenshots,
        )
        .await;
        let final_status = get_checkpoint(app_for_run.clone(), emitted_run.clone())
            .ok()
            .flatten()
            .map(|checkpoint| checkpoint.status)
            .unwrap_or_default();
        cleanup_run_screenshots(
            &app_for_run,
            &emitted_run,
            &screenshot_retention,
            &final_status,
        );
        release_run_lock(&lock_path, &emitted_run);
        if let Ok(mut controls) = app_for_run.state::<RuntimeState>().run_controls.lock() {
            controls.remove(&emitted_run);
        }
        if let Ok(mut notes) = app_for_run.state::<RuntimeState>().steer_notes.lock() {
            notes.remove(&emitted_run);
        }
        if let Ok(mut overrides) = app_for_run.state::<RuntimeState>().approved_overrides.lock() {
            overrides.remove(&emitted_run);
        }
    });
    Ok(run_id)
}

fn run_screenshots_dir(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    let path = app_data_dir(app)?.join("run-screenshots").join(run_id);
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn screenshot_slug(application: &str) -> String {
    application
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn should_keep_screenshots(retention: &str, final_status: &str) -> bool {
    match retention {
        "all" => true,
        "failures" => final_status == "failed",
        _ => false,
    }
}

/// Runs do not survive an app restart, so leftover planner spills, atomic-write
/// temps, and screenshot folders from a crashed session must not accumulate.
fn sweep_stale_session_files(app: &AppHandle) {
    let Ok(root) = app_data_dir(app) else {
        return;
    };
    let _ = fs::remove_dir_all(root.join("run-screenshots"));
    let _ = fs::remove_dir_all(root.join("planner-prompts"));
    let _ = fs::remove_dir_all(root.join("planner-workspace"));
    for directory in [
        root.clone(),
        root.join("checkpoints"),
        root.join("goal-runs"),
        root.join("goal-run-steps"),
        root.join("planner-workspace"),
    ] {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if name.ends_with(".tmp") || name.starts_with('.') && name.contains(".tmp") {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn cleanup_run_screenshots(app: &AppHandle, run_id: &str, retention: &str, final_status: &str) {
    if !should_keep_screenshots(retention, final_status) {
        if let Ok(dir) = app_data_dir(app).map(|dir| dir.join("run-screenshots").join(run_id)) {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// Captures one screenshot per target application into the run's private folder.
/// Returns the file paths (attached to planner turns for vision-capable CLIs)
/// and the first capture as a data URL (shown in the cockpit timeline). Prunes
/// the folder to the newest dozen files so long runs cannot fill the disk.
fn capture_run_screenshots(
    app: &AppHandle,
    run_id: &str,
    applications: &[String],
    step_index: u32,
    pinned_tab: Option<i64>,
) -> (Vec<PathBuf>, Option<String>) {
    use base64::Engine;
    let mut paths = Vec::new();
    let mut evidence = None;
    let Ok(dir) = run_screenshots_dir(app, run_id) else {
        return (paths, evidence);
    };
    for application in applications {
        let captured: Option<(String, Option<String>)> = if application == "Installed browser" {
            let mut params = serde_json::json!({});
            if let (Some(tab), Value::Object(ref mut map)) = (pinned_tab, &mut params) {
                map.insert("tabId".to_string(), Value::from(tab));
            }
            send_browser_command_inner(
                app.clone(),
                BrowserCommand {
                    id: "goal-capture".into(),
                    method: "captureVisible".into(),
                    effect: "observe".into(),
                    intent: "capture the page for planner vision".into(),
                    target_label: None,
                    params,
                    run_id: Some(run_id.to_string()),
                },
                false,
            )
            .ok()
            .and_then(|value| {
                value
                    .get("result")
                    .and_then(|result| result.get("dataUrl"))
                    .and_then(Value::as_str)
                    .map(|data_url| {
                        (
                            data_url
                                .trim_start_matches("data:image/png;base64,")
                                .to_string(),
                            Some(data_url.to_string()),
                        )
                    })
            })
        } else if cfg!(windows) {
            let runtime = app.state::<RuntimeState>();
            resolve_application_process_id(app, &runtime, application)
                .and_then(|pid| {
                    let request = ActionRequest {
                        protocol_version: protocol_version(),
                        run_id: run_id.into(),
                        workflow_step: "goal-capture".into(),
                        application: application.clone(),
                        intent: format!("capture {application} for planner vision"),
                        effect: "observe".into(),
                        target_label: None,
                        payload: Some(serde_json::json!({ "processId": pid })),
                    };
                    execute_native_action_inner(
                        app,
                        &runtime,
                        request,
                        "captureWindow".into(),
                        Duration::from_secs(15),
                        false,
                    )
                })
                .ok()
                .and_then(|value| {
                    value
                        .get("base64")
                        .and_then(Value::as_str)
                        .map(|base64| (base64.to_string(), None))
                })
        } else {
            None
        };
        if let Some((png_base64, data_url)) = captured {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&png_base64) {
                let path = dir.join(format!(
                    "step-{step_index:04}-{}.png",
                    screenshot_slug(application)
                ));
                if fs::write(&path, bytes).is_ok() {
                    paths.push(path);
                    if evidence.is_none() {
                        evidence = Some(
                            data_url
                                .unwrap_or_else(|| format!("data:image/png;base64,{png_base64}")),
                        );
                    }
                }
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&dir) {
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.extension().map(|ext| ext == "png").unwrap_or(false))
            .collect();
        files.sort();
        let excess = files.len().saturating_sub(12);
        for stale in files.into_iter().take(excess) {
            let _ = fs::remove_file(stale);
        }
    }
    (paths, evidence)
}

#[tauri::command]
fn set_run_control(
    state: State<'_, RuntimeState>,
    run_id: String,
    control: String,
) -> Result<(), String> {
    if !["running", "paused", "stop"].contains(&control.as_str()) {
        return Err("Invalid run control.".into());
    }
    let mut controls = state
        .run_controls
        .lock()
        .map_err(|_| "Run control state is unavailable.".to_string())?;
    if !controls.contains_key(&run_id) {
        return Err("The run is no longer active.".into());
    }
    controls.insert(run_id, control);
    Ok(())
}

/// Queues mid-run user guidance for an active goal run. The agent loop drains
/// the queue into the next planner turn's history, so the user can redirect
/// Alfred without pausing or stopping the run.
#[tauri::command]
fn steer_run(app: AppHandle, run_id: String, note: String) -> Result<(), String> {
    let note: String = note.trim().chars().take(500).collect();
    if note.is_empty() {
        return Err("Say something first.".into());
    }
    let state = app.state::<RuntimeState>();
    {
        let controls = state
            .run_controls
            .lock()
            .map_err(|_| "Run control state is unavailable.")?;
        if !controls.contains_key(&run_id) {
            return Err("The run is no longer active.".into());
        }
    }
    let mut notes = state
        .steer_notes
        .lock()
        .map_err(|_| "Steer state is unavailable.")?;
    let queue = notes.entry(run_id).or_default();
    if queue.len() >= 20 {
        queue.remove(0);
    }
    queue.push(note);
    Ok(())
}

#[tauri::command]
fn list_schedules(app: AppHandle) -> Result<Vec<WorkflowSchedule>, String> {
    read_json_or_default(&schedules_path(&app)?)
}

#[cfg(windows)]
fn register_windows_schedule(schedule: &WorkflowSchedule) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let day_names = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];
    let days = schedule
        .days
        .iter()
        .filter_map(|day| day_names.get(*day as usize))
        .copied()
        .collect::<Vec<_>>()
        .join(",");
    let task_name = format!("Alfred-{}", schedule.id);
    let task_run = format!(
        "\"{}\" --run-workflow {}",
        exe.display(),
        schedule.workflow_id
    );
    let start = format!("{:02}:{:02}", schedule.hour, schedule.minute);
    let mut task = Command::new("schtasks");
    hide_windows_console(&mut task);
    let status = task
        .args([
            "/Create", "/SC", "WEEKLY", "/D", &days, "/TN", &task_name, "/TR", &task_run, "/ST",
            &start, "/F",
        ])
        .status()
        .map_err(|error| error.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Windows Task Scheduler rejected the workflow schedule.".into())
    }
}

#[cfg(not(windows))]
fn register_windows_schedule(_schedule: &WorkflowSchedule) -> Result<(), String> {
    Ok(())
}

#[tauri::command]
fn save_schedule(
    app: AppHandle,
    workflow_id: String,
    workflow_name: String,
    hour: u32,
    minute: u32,
    days: Vec<u32>,
) -> Result<WorkflowSchedule, String> {
    if hour > 23 || minute > 59 || days.iter().any(|day| *day > 6) {
        return Err("The schedule contains an invalid time or day.".into());
    }
    let mut schedules: Vec<WorkflowSchedule> = read_json_or_default(&schedules_path(&app)?)?;
    let schedule = WorkflowSchedule {
        id: Uuid::new_v4().to_string(),
        workflow_id,
        workflow_name,
        hour,
        minute,
        days,
        enabled: true,
        last_triggered_key: None,
        created_at: Utc::now(),
    };
    register_windows_schedule(&schedule)?;
    schedules.push(schedule.clone());
    write_json(&schedules_path(&app)?, &schedules)?;
    Ok(schedule)
}

#[tauri::command]
fn set_schedule_enabled(
    app: AppHandle,
    schedule_id: String,
    enabled: bool,
) -> Result<Vec<WorkflowSchedule>, String> {
    let mut schedules: Vec<WorkflowSchedule> = read_json_or_default(&schedules_path(&app)?)?;
    let schedule = schedules
        .iter_mut()
        .find(|item| item.id == schedule_id)
        .ok_or_else(|| "Schedule not found.".to_string())?;
    schedule.enabled = enabled;
    #[cfg(windows)]
    {
        let task_name = format!("Alfred-{}", schedule.id);
        let option = if enabled { "/ENABLE" } else { "/DISABLE" };
        let status = Command::new("schtasks")
            .args(["/Change", "/TN", &task_name, option])
            .status()
            .map_err(|error| error.to_string())?;
        if !status.success() {
            return Err("Windows Task Scheduler could not update the workflow schedule.".into());
        }
    }
    write_json(&schedules_path(&app)?, &schedules)?;
    Ok(schedules)
}

fn start_scheduler(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let now = Local::now();
            let day = now.weekday().num_days_from_monday();
            let key = format!(
                "{:04}-{:02}-{:02}-{:02}-{:02}",
                now.year(),
                now.month(),
                now.day(),
                now.hour(),
                now.minute()
            );
            let path = match schedules_path(&app) {
                Ok(path) => path,
                Err(_) => continue,
            };
            let mut schedules: Vec<WorkflowSchedule> =
                read_json_or_default(&path).unwrap_or_default();
            let mut changed = false;
            let mut due = Vec::new();
            for schedule in schedules.iter_mut() {
                if schedule.enabled
                    && schedule.days.contains(&day)
                    && schedule.hour == now.hour()
                    && schedule.minute == now.minute()
                    && schedule.last_triggered_key.as_deref() != Some(&key)
                {
                    schedule.last_triggered_key = Some(key.clone());
                    changed = true;
                    due.push(schedule.clone());
                    let _ = app.emit("alfred://schedule-due", schedule.clone());
                }
            }
            if changed {
                let _ = write_json(&path, &schedules);
            }
            if let Ok(settings) = get_settings(app.clone()) {
                for schedule in due {
                    if let Ok((_, workflow)) =
                        load_workflow(&settings.library_path, &schedule.workflow_id)
                    {
                        let _ = start_goal_run(
                            app.clone(),
                            workflow.goal,
                            workflow.required_apps,
                            workflow.planner_provider,
                            Some(0),
                        )
                        .await;
                    }
                }
            }
        }
    });
}

#[tauri::command]
fn start_demo_run(app: AppHandle, workflow_id: String) -> String {
    let run_id = Uuid::new_v4().to_string();
    let emitted_run_id = run_id.clone();
    tauri::async_runtime::spawn(async move {
        let steps = [
            (
                "prepare",
                "Preparing workspace",
                "Checking applications and safety permissions",
                "Alfred",
                "running",
                8,
            ),
            (
                "browser",
                "Opening Microsoft Edge",
                "Navigating to the approved workspace",
                "Microsoft Edge",
                "running",
                28,
            ),
            (
                "extract",
                "Reading the invoice table",
                "Found 14 rows and validated the column headers",
                "Microsoft Edge",
                "running",
                52,
            ),
            (
                "policy",
                "Checking the next action",
                "Append-only workbook update approved by the safety engine",
                "Alfred Safety",
                "running",
                68,
            ),
            (
                "workbook",
                "Updating the workbook",
                "Appending 14 new rows without replacing existing data",
                "Microsoft Excel",
                "running",
                86,
            ),
            (
                "verify",
                "Verifying the result",
                "All 14 rows are present and totals match",
                "Microsoft Excel",
                "completed",
                100,
            ),
        ];
        for (sequence, (step_id, title, detail, application, status, progress)) in
            steps.iter().enumerate()
        {
            tokio::time::sleep(std::time::Duration::from_millis(if sequence == 0 {
                250
            } else {
                900
            }))
            .await;
            let _ = app.emit(
                "alfred://run-event",
                RunEvent {
                    run_id: emitted_run_id.clone(),
                    sequence,
                    step_id: (*step_id).into(),
                    title: (*title).into(),
                    detail: (*detail).into(),
                    application: (*application).into(),
                    status: (*status).into(),
                    progress: *progress,
                    evidence_data_url: None,
                    timestamp: Utc::now(),
                },
            );
        }
        let _ = workflow_id;
    });
    run_id
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(RuntimeState::default())
        .setup(|app| {
            sweep_stale_session_files(app.handle());
            start_scheduler(app.handle().clone());
            let args: Vec<String> = std::env::args().collect();
            if let Some(index) = args.iter().position(|value| value == "--run-workflow") {
                if let Some(workflow_id) = args.get(index + 1) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.hide();
                    }
                    if let Ok(settings) = get_settings(app.handle().clone()) {
                        // Scheduled workflows re-enter the live goal loop so app
                        // state is re-observed and stale refs/coordinates are
                        // never replayed blindly.
                        let scheduled = load_workflow(&settings.library_path, workflow_id)
                            .map(|(_, workflow)| workflow)
                            .and_then(|workflow| {
                                tauri::async_runtime::block_on(start_goal_run(
                                    app.handle().clone(),
                                    workflow.goal,
                                    workflow.required_apps,
                                    workflow.planner_provider,
                                    Some(0),
                                ))
                            });
                        match scheduled {
                            Ok(run_id) => {
                                let scheduled_app = app.handle().clone();
                                tauri::async_runtime::spawn(async move {
                                    loop {
                                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                                        if let Ok(Some(checkpoint)) =
                                            get_checkpoint(scheduled_app.clone(), run_id.clone())
                                        {
                                            if ["completed", "failed", "stopped", "waiting"]
                                                .contains(&checkpoint.status.as_str())
                                            {
                                                scheduled_app.exit(
                                                    if checkpoint.status == "completed" {
                                                        0
                                                    } else {
                                                        1
                                                    },
                                                );
                                                break;
                                            }
                                        }
                                    }
                                });
                            }
                            Err(_) => app.handle().exit(1),
                        }
                    }
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_system_info,
            get_settings,
            save_settings,
            detect_providers,
            store_provider_secret,
            has_provider_secret,
            list_workflows,
            list_permissions,
            grant_permission,
            set_permission_enabled,
            evaluate_action,
            get_checkpoint,
            get_goal_run_memory,
            complete_goal_run,
            save_goal_run_as_workflow,
            start_goal_run,
            set_run_control,
            steer_run,
            approve_run_step,
            list_schedules,
            save_schedule,
            set_schedule_enabled,
            browser_bridge_status,
            send_browser_command,
            execute_native_action,
            start_demo_run,
            list_run_logs,
            read_run_log,
            run_logs_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Alfred");
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(intent: &str, effect: &str, target: Option<&str>) -> ActionRequest {
        ActionRequest {
            protocol_version: protocol_version(),
            run_id: "test".into(),
            workflow_step: "one".into(),
            application: "Outlook".into(),
            intent: intent.into(),
            effect: effect.into(),
            target_label: target.map(str::to_string),
            payload: None,
        }
    }
    #[test]
    fn blocks_explicit_deletion() {
        assert_eq!(
            evaluate_base_policy(&request("delete email", "destructive", Some("Delete"))).decision,
            "hard_deny"
        );
    }
    #[test]
    fn blocks_disguised_destructive_targets() {
        assert_eq!(
            evaluate_base_policy(&request("click", "unknown", Some("Empty Trash"))).decision,
            "hard_deny"
        );
    }
    #[test]
    fn blocks_destructive_overwrite() {
        assert_eq!(
            evaluate_base_policy(&request(
                "replace file",
                "modify_reversible",
                Some("Overwrite existing")
            ))
            .decision,
            "hard_deny"
        );
    }
    #[test]
    fn blocks_destructive_payloads() {
        let mut value = request("click", "modify_reversible", Some("Continue"));
        value.payload = Some(serde_json::json!({"button": "Purge records"}));
        assert_eq!(evaluate_base_policy(&value).decision, "hard_deny");
    }
    #[test]
    fn asks_for_unknown_effects() {
        assert_eq!(
            evaluate_base_policy(&request("submit", "unknown", Some("Continue"))).decision,
            "request_user"
        );
    }
    #[test]
    fn allows_scoped_reversible_actions() {
        assert_eq!(
            evaluate_base_policy(&request(
                "append rows",
                "modify_reversible",
                Some("Invoices")
            ))
            .decision,
            "allow"
        );
    }
    #[test]
    fn classifies_known_methods_without_planner_approval_labels() {
        assert_eq!(
            effective_effect("invokeElement", "unknown"),
            "modify_reversible"
        );
        assert_eq!(
            effective_effect("browser.click", "observe"),
            "modify_reversible"
        );
        assert_eq!(effective_effect("observeWindow", "unknown"), "observe");
        assert_eq!(method_effect("launchApplication"), "external_write");
        assert_eq!(method_effect("scroll"), "observe");
        assert_eq!(
            effective_effect("launchApplication", "unknown"),
            "external_write"
        );
        assert_eq!(
            effective_effect("navigateApplication", "observe"),
            "external_write"
        );
        assert_eq!(
            effective_effect("probe", "unknown"),
            "observe"
        );
        assert_eq!(
            method_effect_for(
                "invokeElement",
                Some("Post"),
                Some(&serde_json::json!({"mark": "n12"}))
            ),
            "external_write"
        );
        assert_eq!(
            method_effect_for(
                "invokeElement",
                Some("Format"),
                Some(&serde_json::json!({"mark": "n3"}))
            ),
            "modify_reversible"
        );
    }
    #[test]
    fn allows_reversible_remove_and_draft_rewrite() {
        assert_eq!(
            evaluate_base_policy(&request(
                "delete the selected file",
                "modify_reversible",
                Some("Confirm")
            ))
            .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request(
                "move it to trash",
                "modify_reversible",
                Some("Yes")
            ))
            .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request(
                "click the control",
                "modify_reversible",
                Some("Delete account")
            ))
            .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request(
                "confirm",
                "modify_reversible",
                Some("Destroy account")
            ))
            .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("Purge all data")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("Delete project")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("Remove workspace")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("Trash")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("Overwrite")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("Delete-account")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("remove_user")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request(
                "delete the project after sorting",
                "modify_reversible",
                Some("Confirm")
            ))
            .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request(
                "remove the filter then delete the account",
                "modify_reversible",
                Some("Confirm")
            ))
            .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request("click", "modify_reversible", Some("Delete draft")))
                .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request(
                "remove the filter and the user",
                "modify_reversible",
                Some("Confirm")
            ))
            .decision,
            "hard_deny"
        );
        assert_eq!(
            evaluate_base_policy(&request(
                "delete the project after applying the filter",
                "modify_reversible",
                Some("Yes")
            ))
            .decision,
            "hard_deny"
        );
        assert!(is_reversible_remove("remove the Status filter"));
        assert!(is_reversible_remove("delete the draft text so we can retype"));
        assert!(!is_reversible_remove("Delete draft"));
        assert!(!is_reversible_remove("remove the filter and the user"));
        assert!(!is_reversible_remove("remove the filter then delete the account"));
        assert!(!is_reversible_remove("delete the project after sorting"));
        assert_eq!(
            evaluate_base_policy(&request(
                "remove the Status filter",
                "modify_reversible",
                Some("Clear filter")
            ))
            .decision,
            "allow"
        );
        let mut rewrite = request(
            "delete the draft text so we can retype",
            "modify_reversible",
            Some("Post text"),
        );
        rewrite.payload = Some(serde_json::json!({"text": "A better draft", "mark": "n7"}));
        assert_eq!(evaluate_base_policy(&rewrite).decision, "allow");
    }
    #[test]
    fn native_mark_catalog_is_the_planner_observation() {
        let observation = serde_json::json!({
            "generation": 4,
            "title": "Untitled - Notepad",
            "dpi": 144,
            "focused": "n1",
            "marks": [
                {
                    "id": "n1",
                    "role": "Document",
                    "name": "Text Editor",
                    "automationId": "15",
                    "patterns": ["Value", "Text"],
                    "enabled": true,
                    "chrome": false
                },
                {
                    "id": "n2",
                    "role": "MenuItem",
                    "name": "File",
                    "automationId": "mFile",
                    "patterns": ["Invoke"],
                    "enabled": true,
                    "chrome": false
                }
            ],
            "texts": ["The document body that proves a later publication."]
        });
        let mut lines = Vec::new();
        summarize_native_observation(&observation, "Notepad", &mut lines);
        assert!(lines[0].contains("Notepad  gen=4"));
        assert!(lines.iter().any(|line| line.contains("n1") && line.contains("Document")));
        assert!(lines.iter().any(|line| line.contains("n2") && line.contains("mFile")));
        assert!(lines.iter().any(|line| line.starts_with("text: ") && line.contains("document body")));
        assert!(!lines.iter().any(|line| line.contains("[screen")));
    }
    #[test]
    fn provider_commands_are_restricted() {
        let invocation = provider_invocation("codex", "plan", &[]).unwrap();
        assert!(invocation.args.contains(&"read-only".to_string()));
        assert!(invocation
            .args
            .contains(&"--skip-git-repo-check".to_string()));
        assert!(invocation
            .args
            .contains(&"--ignore-user-config".to_string()));
        assert!(invocation.args.contains(&"never".to_string()));
        assert_eq!(invocation.args.last().map(String::as_str), Some("-"));
        assert_eq!(invocation.stdin.as_deref(), Some("plan"));
    }
    #[test]
    fn provider_turns_resume_the_exact_session() {
        let session = "123e4567-e89b-12d3-a456-426614174000";
        let codex =
            provider_invocation_for_session("codex", "next", &[], Some(session), true).unwrap();
        assert!(codex
            .args
            .windows(2)
            .any(|pair| pair[0] == "exec" && pair[1] == "resume"));
        assert!(codex.args.iter().any(|arg| arg == session));
        assert!(!codex.args.iter().any(|arg| arg == "--ephemeral"));

        let cursor =
            provider_invocation_for_session("cursor", "next", &[], Some(session), true).unwrap();
        assert!(cursor
            .args
            .iter()
            .any(|arg| arg == &format!("--resume={session}")));

        let grok_first =
            provider_invocation_for_session("grok", "first", &[], Some(session), false).unwrap();
        assert!(grok_first
            .args
            .windows(2)
            .any(|pair| pair[0] == "--session-id" && pair[1] == session));
        let grok_next =
            provider_invocation_for_session("grok", "next", &[], Some(session), true).unwrap();
        assert!(grok_next
            .args
            .windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == session));

        let copilot =
            provider_invocation_for_session("copilot", "next", &[], Some(session), true).unwrap();
        assert!(copilot
            .args
            .windows(2)
            .any(|pair| pair[0] == "--session-id" && pair[1] == session));
        assert!(copilot.args.iter().any(|arg| arg.contains("deny-tool")));
        assert!(copilot
            .args
            .iter()
            .any(|arg| arg == "--no-custom-instructions"));
    }
    #[test]
    fn extracts_documented_provider_session_ids() {
        let codex = r#"{"type":"thread.started","thread_id":"codex-thread"}"#;
        assert_eq!(
            provider_session_id_from_output("codex", codex).as_deref(),
            Some("codex-thread")
        );
        let cursor = r#"{"type":"system","session_id":"cursor-session","request_id":"ignore"}"#;
        assert_eq!(
            provider_session_id_from_output("cursor", cursor).as_deref(),
            Some("cursor-session")
        );
    }

    #[test]
    fn treats_zero_exit_authentication_diagnostics_as_provider_errors() {
        let output = "Error: No authentication information found. Run the '/login' command.";
        let error = provider_output_error("copilot", output).unwrap();
        assert!(error.contains("not authenticated"));
        assert!(provider_output_error("copilot", r#"{"done":true}"#).is_none());
    }

    #[test]
    fn recognizes_windows_command_wrappers() {
        assert!(is_windows_command_script(Path::new("C:/npm/codex.cmd")));
        assert!(is_windows_command_script(Path::new("C:/npm/codex.BAT")));
        assert!(!is_windows_command_script(Path::new("C:/bin/codex.exe")));
    }
    #[test]
    fn ignores_extensionless_windows_npm_shim() {
        let selected = select_resolved_command(
            vec![
                PathBuf::from("C:/Users/test/AppData/Roaming/npm/codex"),
                PathBuf::from("C:/Users/test/AppData/Roaming/npm/codex.cmd"),
            ],
            true,
        );
        assert_eq!(
            selected.as_deref(),
            Some(Path::new("C:/Users/test/AppData/Roaming/npm/codex.cmd"))
        );
    }
    #[test]
    fn builds_cmd_wrapper_without_c_runtime_quote_escaping() {
        let resolved = windows_command_script_process(
            Path::new("C:/Users/Test User/AppData/Roaming/npm/codex.cmd"),
            &["exec".into(), "--json".into()],
            true,
        )
        .unwrap();
        assert_eq!(resolved.args, ["/D", "/S", "/C"]);
        assert_eq!(
            resolved.windows_raw_argument.as_deref(),
            Some("\"\"C:/Users/Test User/AppData/Roaming/npm/codex.cmd\" \"exec\" \"--json\"\"")
        );
        assert!(!resolved
            .windows_raw_argument
            .as_deref()
            .unwrap()
            .contains("\\\""));
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn executes_cmd_wrapper_from_path_with_spaces() {
        let directory =
            std::env::temp_dir().join(format!("Alfred provider wrapper {}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let script = directory.join("codex.cmd");
        fs::write(&script, "@echo off\r\necho [%~1][%~2]\r\n").unwrap();
        let resolved =
            windows_command_script_process(&script, &["alpha beta".into(), "gamma".into()], true)
                .unwrap();
        let mut process = Command::new(resolved.program);
        process.args(resolved.args);
        process.raw_arg(resolved.windows_raw_argument.unwrap());
        let output = process.output().unwrap();
        let _ = fs::remove_dir_all(&directory);
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "[alpha beta][gamma]"
        );
    }
    #[test]
    fn parses_codex_jsonl_plan() {
        let output = vec![serde_json::json!({
            "type": "item.completed",
            "item": {
                "type": "agent_message",
                "text": "{\"steps\":[{\"title\":\"Open Notepad\",\"application\":\"Notepad\",\"method\":\"launchApplication\",\"targetLabel\":\"Notepad\",\"params\":{}},{\"title\":\"Type greeting\",\"application\":\"Notepad\",\"method\":\"typeText\",\"targetLabel\":\"Editor\",\"params\":{\"text\":\"Hello from Alfred\"}}]}"
            }
        })
        .to_string()];
        let steps = parse_provider_plan_output(&output).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].kind, "launchApplication");
        assert_eq!(steps[1].kind, "typeText");
    }
    #[test]
    fn rejects_destructive_provider_plan() {
        let output = vec![
            "{\"steps\":[{\"title\":\"Delete note\",\"application\":\"Notepad\",\"method\":\"invokeElement\",\"targetLabel\":\"Delete\",\"params\":{}}]}".into(),
        ];
        assert!(parse_provider_plan_output(&output)
            .unwrap_err()
            .contains("blocked"));
    }
    #[test]
    fn accepts_named_app_launch_but_rejects_delete_key() {
        let installed_launch = WorkflowStep {
            id: "one".into(),
            title: "Open PowerShell".into(),
            kind: "launchApplication".into(),
            effect: "external_write".into(),
            application: Some("PowerShell".into()),
            intent: Some("launch application".into()),
            target_label: Some("PowerShell".into()),
            payload: Some(serde_json::json!({})),
            timeout_ms: default_timeout(),
            retries: default_retries(),
            wait_for: None,
            expect: None,
            save_as: None,
        };
        // Core accepts the semantic application name. The Windows host permits
        // it only when it exactly matches an installed Start-menu shortcut.
        assert!(validate_workflow_step(&installed_launch).is_ok());

        let delete_key = WorkflowStep {
            id: "two".into(),
            title: "Press Delete".into(),
            kind: "key".into(),
            effect: "modify_reversible".into(),
            application: Some("Notepad".into()),
            intent: Some("press key".into()),
            target_label: Some("Editor".into()),
            payload: Some(serde_json::json!({"virtualKey": 46})),
            timeout_ms: default_timeout(),
            retries: default_retries(),
            wait_for: None,
            expect: None,
            save_as: None,
        };
        assert!(validate_workflow_step(&delete_key)
            .unwrap_err()
            .contains("Delete key"));
    }
    #[test]
    fn codex_attaches_images_before_the_prompt() {
        let images = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.png")];
        let invocation = provider_invocation("codex", "plan", &images).unwrap();
        let flag = invocation
            .args
            .iter()
            .position(|arg| arg == "--image")
            .unwrap();
        assert_eq!(invocation.args[flag + 1], "/tmp/a.png,/tmp/b.png");
        // The prompt travels over stdin; "-" stays the final argument.
        assert_eq!(invocation.args.last().map(String::as_str), Some("-"));
        assert_eq!(invocation.stdin.as_deref(), Some("plan"));
        // No images, no flag.
        let plain = provider_invocation("codex", "plan", &[]).unwrap();
        assert!(!plain.args.iter().any(|arg| arg == "--image"));
    }
    #[test]
    fn copilot_attaches_images_as_attachments() {
        let images = vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.png")];
        let invocation = provider_invocation("copilot", "plan", &images).unwrap();
        let flags: Vec<_> = invocation
            .args
            .iter()
            .filter(|arg| arg.as_str() == "--attachment")
            .collect();
        assert_eq!(flags.len(), 2);
        assert!(invocation.args.iter().any(|arg| arg == "/tmp/a.png"));
        assert!(invocation.args.iter().any(|arg| arg == "/tmp/b.png"));
        // Path-delivery providers keep clean args; their pipe is the prompt text.
        let grok = provider_invocation("grok", "plan", &images).unwrap();
        assert!(!grok.args.iter().any(|arg| arg.contains("a.png")));
        assert!(matches!(
            provider_image_delivery("codex"),
            Some(ImageDelivery::Flag)
        ));
        assert!(matches!(
            provider_image_delivery("copilot"),
            Some(ImageDelivery::Flag)
        ));
        assert!(matches!(
            provider_image_delivery("grok"),
            Some(ImageDelivery::PromptPaths)
        ));
        assert!(matches!(
            provider_image_delivery("cursor"),
            Some(ImageDelivery::PromptPaths)
        ));
    }
    #[test]
    fn launches_skip_process_resolution() {
        // The app is intentionally not running when a launch step executes.
        assert!(!needs_process_resolution("launchApplication", "Notepad"));
        assert!(!needs_process_resolution("listApplications", "Alfred"));
        assert!(!needs_process_resolution("listInstalledApplications", "Alfred"));
        assert!(needs_process_resolution("typeText", "Notepad"));
        assert!(needs_process_resolution("focusApplication", "Notepad"));
        assert!(!needs_process_resolution("typeText", "Alfred"));
    }
    #[test]
    fn inventory_methods_are_allowed_observe_actions() {
        let list_apps = WorkflowStep {
            id: "list".into(),
            title: "See running apps".into(),
            kind: "listApplications".into(),
            effect: "observe".into(),
            application: Some("Alfred".into()),
            intent: Some("list running windows".into()),
            target_label: None,
            payload: None,
            timeout_ms: default_timeout(),
            retries: default_retries(),
            wait_for: None,
            expect: None,
            save_as: None,
        };
        assert!(validate_workflow_step(&list_apps).is_ok());
        assert_eq!(method_effect("listApplications"), "observe");
        assert_eq!(method_effect("activate"), "modify_reversible");
        assert!(ALLOWED_PLAN_METHODS.contains(&"listApplications"));
        assert!(ALLOWED_PLAN_METHODS.contains(&"activate"));
    }
    #[test]
    fn mutating_methods_cannot_masquerade_as_observe() {
        // A prompt-injected planner (or hand-edited YAML) declaring "observe" on a
        // mutating method must not skip the permission grant.
        assert_eq!(effective_effect("typeText", "observe"), "modify_reversible");
        assert_eq!(effective_effect("setValue", "observe"), "modify_reversible");
        assert_eq!(
            effective_effect("browser.click", "observe"),
            "modify_reversible"
        );
        assert_eq!(effective_effect("observeWindow", "observe"), "observe");
        assert_eq!(effective_effect("browser.observe", "observe"), "observe");
        assert_eq!(effective_effect("getValue", "observe"), "observe");
        assert_eq!(
            effective_effect("typeText", "modify_reversible"),
            "modify_reversible"
        );
    }
    #[test]
    fn screenshot_retention_policy() {
        assert!(should_keep_screenshots("all", "completed"));
        assert!(should_keep_screenshots("failures", "failed"));
        assert!(!should_keep_screenshots("failures", "completed"));
        assert!(!should_keep_screenshots("none", "failed"));
        assert_eq!(screenshot_slug("Microsoft Edge"), "microsoft-edge");
        assert_eq!(screenshot_slug("Installed browser"), "installed-browser");
    }

    #[test]
    fn atomic_write_uses_unique_temp_and_cleans_it_up() {
        let directory = std::env::temp_dir().join(format!("alfred-atomic-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.json");
        atomic_write(&path, br#"{"ok":true}"#).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"ok":true}"#);
        let leftovers: Vec<_> = fs::read_dir(&directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".tmp")
            })
            .collect();
        let _ = fs::remove_dir_all(&directory);
        assert!(leftovers.is_empty(), "atomic_write left temp files: {leftovers:?}");
    }
    #[test]
    fn blocks_delete_virtual_key_payload() {
        // The keyword filters cannot see that virtual-key 0x2E is the Delete key;
        // the policy must deny it through the non-semantic channel.
        let mut value = request("press key", "modify_reversible", Some("File list"));
        value.payload = Some(serde_json::json!({"virtualKey": 46}));
        assert_eq!(evaluate_base_policy(&value).decision, "hard_deny");
    }
    #[test]
    fn allows_navigation_virtual_key_payload() {
        let mut value = request("press enter", "modify_reversible", Some("OK"));
        value.payload = Some(serde_json::json!({"virtualKey": 13}));
        assert_eq!(evaluate_base_policy(&value).decision, "allow");
    }
    #[test]
    fn run_lock_rejects_overlap_and_allows_resume_and_stale_takeover() {
        let path = std::env::temp_dir().join(format!("alfred-test-lock-{}.json", Uuid::new_v4()));
        try_acquire_run_lock(&path, "run-a").unwrap();
        // A second, different run is refused while the first is active.
        assert!(try_acquire_run_lock(&path, "run-b").is_err());
        // The same run may re-acquire (checkpoint resume).
        assert!(try_acquire_run_lock(&path, "run-a").is_ok());
        // A stale lock (crashed owner) can be taken over.
        let stale = Utc::now() - chrono::Duration::minutes(RUN_LOCK_STALE_MINUTES + 5);
        fs::write(
            &path,
            serde_json::json!({"runId": "run-c", "updatedAt": stale.to_rfc3339()}).to_string(),
        )
        .unwrap();
        assert!(try_acquire_run_lock(&path, "run-b").is_ok());
        // Only the owner releases the lock.
        release_run_lock(&path, "run-a");
        assert!(path.exists());
        release_run_lock(&path, "run-b");
        assert!(!path.exists());
    }
    #[test]
    fn substitutes_variables_in_nested_payloads() {
        let mut variables = HashMap::new();
        variables.insert("invoice".to_string(), "INV-1042".to_string());
        let mut payload = serde_json::json!({
            "text": "Invoice ${invoice}",
            "steps": ["${invoice}", 42, true],
            "nested": { "url": "https://example.test/${invoice}" }
        });
        substitute_variables(&mut payload, &variables);
        assert_eq!(payload["text"], "Invoice INV-1042");
        assert_eq!(payload["steps"][0], "INV-1042");
        assert_eq!(payload["steps"][1], 42);
        assert_eq!(payload["nested"]["url"], "https://example.test/INV-1042");
        // Unknown placeholders are left untouched so the failure stays visible.
        let mut unknown = serde_json::json!({"text": "${missing}"});
        substitute_variables(&mut unknown, &variables);
        assert_eq!(unknown["text"], "${missing}");
    }
    #[test]
    fn extracts_saved_values_from_native_and_browser_results() {
        let native = serde_json::json!({ "value": "hello" });
        assert_eq!(extract_saved_value(&native).as_deref(), Some("hello"));
        let browser = serde_json::json!({ "ok": true, "result": { "text": "page text" } });
        assert_eq!(extract_saved_value(&browser).as_deref(), Some("page text"));
        let nothing = serde_json::json!({ "clicked": true });
        assert_eq!(extract_saved_value(&nothing), None);
    }
    #[test]
    fn step_conditions_and_save_as_default_off_in_old_workflows() {
        // Workflows recorded before phase 2 must still deserialize unchanged.
        let legacy = serde_json::json!({
            "id": "one", "title": "Click OK", "kind": "click", "effect": "modify_reversible"
        });
        let step: WorkflowStep = serde_json::from_value(legacy).unwrap();
        assert!(step.wait_for.is_none());
        assert!(step.expect.is_none());
        assert!(step.save_as.is_none());
    }
    #[test]
    fn parses_clean_planner_json() {
        let reply = parse_planner_action(
            "{\"done\": false, \"kind\": \"invokeElement\", \"application\": \"Notepad\", \"effect\": \"modify_reversible\", \"payload\": {\"name\": \"Format\"}}",
        )
        .unwrap();
        assert!(!reply.done);
        assert_eq!(reply.kind.as_deref(), Some("invokeElement"));
        assert_eq!(reply.application.as_deref(), Some("Notepad"));
    }
    #[test]
    fn parses_planner_json_wrapped_in_prose_and_fences() {
        let output = "Here is my next action:\n```json\n{\"done\": false, \"kind\": \"key\", \"payload\": {\"virtualKey\": 13}}\n```\nThat presses Enter.";
        let reply = parse_planner_action(output).unwrap();
        assert_eq!(reply.kind.as_deref(), Some("key"));
        let done = parse_planner_action(
            "All finished.\n{\"done\": true, \"summary\": \"Saved the file.\"}",
        )
        .unwrap();
        assert!(done.done);
        assert_eq!(done.summary.as_deref(), Some("Saved the file."));
    }
    #[test]
    fn completion_requires_explicit_nonempty_evidence() {
        let claim = parse_planner_action(r#"{"done":true,"summary":"Saved the file."}"#).unwrap();
        let current = "Notepad: title Alfred smoke.txt\n- Text \"Saved as Alfred smoke.txt\"";
        assert!(!is_verified_completion(&claim, current, None, false));
        let empty = parse_planner_action(
            r#"{"done":true,"verified":true,"summary":"Saved.","evidence":[]}"#,
        )
        .unwrap();
        assert!(!is_verified_completion(&empty, current, None, false));
        let verified = parse_planner_action(
            r#"{"done":true,"verified":true,"summary":"Saved.","evidence":["Notepad title shows Alfred smoke.txt"]}"#,
        )
        .unwrap();
        assert!(is_verified_completion(&verified, current, None, false));
        let hallucinated = parse_planner_action(
            r#"{"done":true,"verified":true,"summary":"Saved.","evidence":["Excel shows quarterly revenue 42 million"]}"#,
        )
        .unwrap();
        assert!(!is_verified_completion(&hallucinated, current, None, false));
        let mut prompt = String::from("state");
        append_completion_review(&mut prompt, "Saved the file", None, false, false);
        assert!(prompt.contains("do not trust the earlier claim"));
        assert!(prompt.contains("CURRENT DESKTOP STATE"));
    }

    #[test]
    fn publication_completion_requires_authored_text_in_static_current_state() {
        let reply = parse_planner_action(
            r#"{"done":true,"verified":true,"summary":"Tweet published.","evidence":["Profile shows Octopuses have three hearts"]}"#,
        )
        .unwrap();
        let text = "Fun fact: Octopuses have three hearts and two stop beating when they swim.";
        let published = "Microsoft Edge:\n- Text \"Fun fact: Octopuses have three hearts and two stop beating when they swim.\"";
        assert!(is_verified_completion(&reply, published, Some(text), true));

        let draft = "Microsoft Edge:\n- Edit \"Fun fact: Octopuses have three hearts and two stop beating when they swim.\"\n- Button \"Post\"";
        assert!(!is_verified_completion(&reply, draft, Some(text), true));

        let someone_elses_similar_post = "Microsoft Edge:\n- Text \"Fun fact: Octopuses have three hearts, blue blood, and two stop beating when they swim.\"";
        assert!(!is_verified_completion(
            &reply,
            someone_elses_similar_post,
            Some(text),
            true
        ));

        let action_receipt =
            "Click Post button (click) — ok\nConfirm tweet text (findElement) — ok";
        assert!(!is_verified_completion(
            &reply,
            action_receipt,
            Some(text),
            true
        ));

        let mut prompt = String::from("state");
        append_completion_review(&mut prompt, "Tweet published", Some(text), true, false);
        assert!(prompt.contains("REQUIRED PUBLISHED TEXT"));
        assert!(prompt.contains("non-editable published content"));

        let mark_catalog = "Microsoft Edge  gen=4  dpi=144  focused=n3\nn3  Hyperlink \"Fun fact: Octopuses have three hearts and two stop beating when they swim.\"\ntext: Fun fact: Octopuses have three hearts and two stop beating when they swim.";
        assert!(is_verified_completion(&reply, mark_catalog, Some(text), true));
        let mark_draft = "Microsoft Edge  gen=4  focused=n1\nn1  Document \"Fun fact: Octopuses have three hearts and two stop beating when they swim.\"  value+text";
        assert!(!is_verified_completion(&reply, mark_draft, Some(text), true));
        let draft_plus_text = "Microsoft Edge  gen=4  focused=n1\nn1  Document \"Fun fact: Octopuses have three hearts and two stop beating when they swim.\"  value+text\ntext: Fun fact: Octopuses have three hearts and two stop beating when they swim.";
        assert!(
            !is_verified_completion(&reply, draft_plus_text, Some(text), true),
            "a sibling text: line must not prove publication while the composer still holds the draft"
        );
        let draft_plus_prose = "Installed browser:\n- Document e1 \"Fun fact: Octopuses have three hearts and two stop beating when they swim.\"\nread-prose: Fun fact: Octopuses have three hearts and two stop beating when they swim.";
        assert!(!is_verified_completion(&reply, draft_plus_prose, Some(text), true));
        let published_link = "Installed browser:\n- a e15 \"Fun fact: Octopuses have three hearts and two stop beating when they swim.\"";
        assert!(is_verified_completion(&reply, published_link, Some(text), true));
        let sanitized_feed = "Installed browser:\npage: https://x.com/you\nread-prose: Fun fact: Octopuses have three hearts and two stop beating when they swim.";
        assert!(is_verified_completion(&reply, sanitized_feed, Some(text), true));
    }
    #[test]
    fn save_goals_need_a_visible_transition() {
        assert!(goal_requires_save_proof("Save the file to the Desktop"));
        assert!(!goal_requires_save_proof("post a tweet"));
        let before = "Notepad  gen=2\ntitle: Untitled - Notepad";
        let already_open = "Excel  gen=3\ntitle: report.xlsx";
        assert!(!observation_shows_save_transition(
            already_open,
            Some(already_open),
            None,
            false,
            None
        ));
        assert!(observation_shows_save_transition(
            "Notepad  gen=3\ntitle: hello.txt",
            Some(before),
            Some("hello.txt"),
            true,
            Some("Notepad")
        ));
        assert!(observation_shows_save_transition(
            "Notepad  gen=3\ntitle: notes.txt",
            Some("Notepad  gen=2\ntitle: *notes.txt"),
            None,
            true,
            Some("Notepad")
        ));
        assert!(
            !observation_shows_save_transition(
                already_open,
                Some(already_open),
                None,
                true,
                Some("Excel")
            ),
            "an unchanged named title is not save proof"
        );
        assert!(
            !observation_shows_save_transition(
                before,
                Some(before),
                None,
                true,
                Some("Notepad")
            ),
            "Ctrl+S that leaves Untitled unchanged is not a save"
        );
        assert!(!goal_requires_save_proof(
            "Save me time by summarizing this document"
        ));
        assert!(!observation_shows_save_transition(
            "Notepad  gen=3\ntitle: hello.txt",
            None,
            Some("hello.txt"),
            true,
            Some("Notepad")
        ));
        assert!(
            !observation_shows_save_transition(
                "Notepad  gen=3\ntitle: Untitled - Notepad\nn4  Edit \"hello.txt\"",
                Some(before),
                Some("hello.txt"),
                true,
                Some("Notepad")
            ),
            "a typed Save As filename is not a committed title"
        );
        assert!(
            !observation_shows_save_transition(
                "Notepad  gen=3\ntitle: hello.txt",
                Some(before),
                Some("hello.txt"),
                false,
                Some("Notepad")
            ),
            "filename visibility without a save commit is not proof"
        );
        let multi = "Installed browser:\npage: https://example.test\ntitle: Example\nNotepad  gen=4  dpi=144  focused=n1\ntitle: hello.txt";
        let multi_before = "Installed browser:\npage: https://example.test\ntitle: Example\nNotepad  gen=2  dpi=144  focused=n1\ntitle: Untitled - Notepad";
        assert!(observation_shows_save_transition(
            multi,
            Some(multi_before),
            Some("hello.txt"),
            true,
            Some("Notepad")
        ));
    }
    #[test]
    fn live_resolved_commit_labels_are_publish_or_save() {
        assert!(is_publish_commit_label("Post"));
        assert!(is_publish_commit_label("Send tweet"));
        assert!(!is_publish_commit_label("Send"));
        assert!(!is_publish_commit_label("Send invite"));
        assert!(!is_publish_commit_label("Send file"));
        assert!(!is_publish_commit_label("Continue"));
        assert!(!is_publish_commit_label("Post text"));
        assert!(!is_publish_commit_label("Share screen"));
        assert!(is_save_commit(
            "invokeElement",
            Some("Save"),
            Some(&serde_json::json!({}))
        ));
        assert!(!is_save_commit(
            "invokeElement",
            Some("Save As"),
            Some(&serde_json::json!({}))
        ));
        assert!(!is_save_commit(
            "invokeElement",
            Some("Don't Save"),
            Some(&serde_json::json!({}))
        ));
        assert!(!is_save_commit(
            "click",
            Some("Do not save"),
            Some(&serde_json::json!({}))
        ));
        assert!(is_save_commit(
            "shortcut",
            None,
            Some(&serde_json::json!({"keys": "CTRL+S"}))
        ));
        assert_eq!(
            live_resolved_label(&serde_json::json!({"targetName": "Post"})).as_deref(),
            Some("Post")
        );
    }

    #[test]
    fn live_goal_progress_never_implies_completion() {
        assert_eq!(goal_run_progress(0), 0);
        assert_eq!(goal_run_progress(10), 40);
        assert_eq!(goal_run_progress(1000), 95);
    }
    #[test]
    fn rejects_planner_output_without_an_action() {
        assert!(parse_planner_action("Sure, here is some unrelated chatter.").is_err());
        assert!(parse_planner_action("{\"message\": \"no action here\"}").is_err());
    }
    #[test]
    fn accepts_method_alias_and_snake_case_planner_json() {
        let reply = parse_planner_action(
            r#"{"done": false, "method": "launchApplication", "application": "Microsoft Edge", "effect": "create", "params": {}}"#,
        )
        .unwrap();
        assert_eq!(reply.kind.as_deref(), Some("launchApplication"));
        assert_eq!(reply.application.as_deref(), Some("Microsoft Edge"));
    }
    #[test]
    fn accepts_nested_action_object_from_stream() {
        let reply = parse_planner_action(
            r#"{"type":"assistant","action":{"done":false,"kind":"browser.navigate","application":"Installed browser","effect":"modify_reversible","payload":{"url":"https://x.com"}}}"#,
        )
        .unwrap();
        assert_eq!(reply.kind.as_deref(), Some("browser.navigate"));
    }
    #[test]
    fn prose_refusal_ends_goal_cleanly_instead_of_spinning() {
        let reply = parse_planner_action(
            "I'm sorry, I can't help with posting automated tweets on social media.",
        )
        .unwrap();
        assert!(reply.done);
        assert!(
            reply
                .summary
                .as_ref()
                .unwrap()
                .to_lowercase()
                .contains("can't help")
                || reply
                    .summary
                    .as_ref()
                    .unwrap()
                    .to_lowercase()
                    .contains("posting")
        );
    }
    #[test]
    fn compacts_oversized_observations_for_cli_planners() {
        let huge = "x".repeat(5000);
        let compact = compact_planner_observations(&huge);
        assert!(compact.chars().count() < 5000);
        assert!(compact.contains("truncated"));
    }
    #[test]
    fn parses_planner_reply_inside_grok_streaming_json_envelope() {
        // Grok/Cursor stream-json: the answer is a string inside content blocks,
        // surrounded by init/result event lines.
        let answer = "{\"done\": false, \"kind\": \"launchApplication\", \"application\": \"Notepad\", \"effect\": \"create\", \"payload\": {}}";
        let output = format!(
            "{}\n{}\n{}",
            serde_json::json!({"type": "system", "subtype": "init", "model": "grok-code-fast-1"}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": answer}]}}),
            serde_json::json!({"type": "result", "subtype": "success", "result": "Done."})
        );
        let reply = parse_planner_action(&output).unwrap();
        assert_eq!(reply.kind.as_deref(), Some("launchApplication"));
        assert_eq!(reply.application.as_deref(), Some("Notepad"));
    }
    #[test]
    fn reassembles_grok_token_streamed_text_events() {
        // Real Grok Build streaming-json: the action JSON is split across dozens
        // of {"type":"text","data":"…"} token events (captured in a live run).
        let chunks = [
            "{\"done\": false, ",
            "\"title\": \"Launch Microsoft Edge\", ",
            "\"kind\": \"launchApplication\", ",
            "\"application\": \"Microsoft Edge\", ",
            "\"intent\": \"Open Edge\", ",
            "\"effect\": \"create\", ",
            "\"targetLabel\": null, ",
            "\"payload\": {}}",
        ];
        let mut output = String::from(
            r#"{"type":"available_commands","tools":[]}
{"type":"thought","data":"planning"}
"#,
        );
        for chunk in chunks {
            output.push_str(&serde_json::json!({"type": "text", "data": chunk}).to_string());
            output.push('\n');
        }
        output.push_str(r#"{"type":"end","stopReason":"end_turn"}"#);
        let reply = parse_planner_action(&output).unwrap();
        assert_eq!(reply.kind.as_deref(), Some("launchApplication"));
        assert_eq!(reply.application.as_deref(), Some("Microsoft Edge"));
        assert!(!reply.done);
    }
    #[test]
    fn parses_grok_whole_message_json_envelope() {
        // grok --output-format json: one object with a complete `text` field.
        let output = r#"{
  "text": "{\"done\": false, \"kind\": \"browser.navigate\", \"application\": \"Installed browser\", \"effect\": \"modify_reversible\", \"payload\": {\"url\": \"https://x.com\"}}",
  "stopReason": "end_turn",
  "usage": {"input_tokens": 100}
}"#;
        let reply = parse_planner_action(output).unwrap();
        assert_eq!(reply.kind.as_deref(), Some("browser.navigate"));
        assert_eq!(
            reply
                .payload
                .as_ref()
                .and_then(|value| value.get("url"))
                .and_then(Value::as_str),
            Some("https://x.com")
        );
    }
    #[test]
    fn parses_live_captured_grok_streaming_tweet_goal() {
        // Captured from a real `grok -p … --output-format streaming-json` run for
        // "launch edge browser and post a tweet on x.com" (token-chunk stream).
        let output = include_str!("../tests/fixtures/grok-streaming-json-tweet-goal.jsonl");
        let reply = parse_planner_action(output).expect("should reassemble live Grok stream");
        assert_eq!(reply.kind.as_deref(), Some("launchApplication"));
        assert_eq!(reply.application.as_deref(), Some("Microsoft Edge"));
    }
    #[test]
    fn parses_live_captured_grok_json_tweet_goal() {
        let output = include_str!("../tests/fixtures/grok-json-tweet-goal.json");
        let reply = parse_planner_action(output).expect("should parse live Grok json envelope");
        assert_eq!(reply.kind.as_deref(), Some("launchApplication"));
        assert_eq!(reply.application.as_deref(), Some("Microsoft Edge"));
    }
    #[test]
    fn grok_provider_uses_whole_message_json_format() {
        let invocation = provider_invocation("grok", "plan", &[]).unwrap();
        assert!(invocation.args.iter().any(|arg| arg == "json"));
        assert!(!invocation.args.iter().any(|arg| arg == "streaming-json"));
    }
    #[test]
    fn retest_post_fix_live_grok_json_and_stream() {
        let json = include_str!("../tests/fixtures/retest-live-json.out");
        let reply = parse_planner_action(json).expect("NEW format json must parse after fix");
        assert_eq!(reply.kind.as_deref(), Some("launchApplication"));
        assert_eq!(reply.application.as_deref(), Some("Microsoft Edge"));
        assert!(!reply.done);

        let stream = include_str!("../tests/fixtures/retest-live-stream.out");
        let reply2 =
            parse_planner_action(stream).expect("streaming-json must reassemble after fix");
        assert_eq!(reply2.kind.as_deref(), Some("launchApplication"));
        assert!(
            reply2.application.as_deref() == Some("Microsoft Edge")
                || reply2.application.as_deref() == Some("Installed browser")
        );
    }

    #[test]
    fn parses_planner_reply_inside_codex_jsonl_envelope() {
        let output = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "agent_message", "text": "{\"done\": true, \"summary\": \"Typed the greeting.\"}" }
        })
        .to_string();
        let reply = parse_planner_action(&output).unwrap();
        assert!(reply.done);
        assert_eq!(reply.summary.as_deref(), Some("Typed the greeting."));
    }
    #[test]
    fn infers_target_applications_from_goal_text() {
        assert_eq!(
            infer_applications_from_goal("Can you open Notepad and type Hello From Alfred"),
            vec!["Notepad".to_string()]
        );
        let apps = infer_applications_from_goal("Copy the table from the website into Excel");
        assert!(apps.contains(&"Microsoft Edge".to_string()));
        assert!(apps.contains(&"Microsoft Excel".to_string()));
        assert_eq!(
            infer_applications_from_goal("Open our Datadog portal and check the RUM errors"),
            vec!["Microsoft Edge".to_string()]
        );
        assert_eq!(
            infer_applications_from_goal("Post a tweet on X"),
            vec!["Microsoft Edge".to_string()]
        );
        assert_eq!(
            infer_applications_from_goal("Send an email to the team"),
            vec!["Microsoft Outlook".to_string()]
        );
        assert_eq!(
            infer_applications_from_goal("Open Gmail and send the draft"),
            vec!["Microsoft Edge".to_string()]
        );
        assert!(infer_applications_from_goal("organize my thoughts").is_empty());
    }
    #[test]
    fn planner_prompt_guides_app_choice_when_none_listed() {
        let prompt = build_planner_prompt(
            "Type hello somewhere safe",
            &[],
            "(no observations available)",
            &[],
            &[],
            false,
        );
        assert!(prompt.contains("infer them from the goal"));
        assert!(prompt.contains("listApplications"));
    }
    #[test]
    fn accepts_planner_plan_outline_replies() {
        let reply = parse_planner_action(
            "{\"plan\": [\"Open Datadog\", \"Open RUM\", \"Read the error list\", \"Summarize\"]}",
        )
        .unwrap();
        assert!(!reply.done);
        assert!(reply.kind.is_none());
        assert_eq!(reply.plan.as_ref().map(|plan| plan.len()), Some(4));
    }
    #[test]
    fn planner_history_digests_read_results() {
        let read = serde_json::json!({"ok": true, "result": {"text": "Error rate spiked\n  to 4.2% of sessions"}});
        assert_eq!(
            planner_result_digest(&read),
            ": Error rate spiked to 4.2% of sessions"
        );
        let click = serde_json::json!({"ok": true, "result": {"clicked": true}});
        assert_eq!(planner_result_digest(&click), "");
        assert!(payload_has_normalized_point(Some(&serde_json::json!({"nx": 0.4, "ny": 0.6}))));
        assert!(!payload_has_normalized_point(Some(&serde_json::json!({"nx": null, "ny": null, "x": 900, "y": 600}))));
        let prose_only = serde_json::json!({
            "ok": true,
            "result": {
                "text": "",
                "prose": "Octopuses have three hearts and live in the deep.",
                "hasMore": true,
                "nextOffset": 6000
            }
        });
        let digest = planner_result_digest(&prose_only);
        assert!(digest.contains("Octopuses have three hearts"));
        assert!(digest.contains("hasMore"));
        assert!(digest.contains("6000"));
    }
    #[test]
    fn planner_prompt_pins_the_working_plan() {
        let prompt = build_planner_prompt(
            "Analyse the RUM errors",
            &["Installed browser".to_string()],
            "Installed browser:\npage: https://app.datadoghq.test",
            &[],
            &["Open Datadog".to_string(), "Open RUM".to_string()],
            true,
        );
        assert!(prompt.contains("CURRENT PLAN"));
        assert!(prompt.contains("2. Open RUM"));
        assert!(prompt.contains("\"plan\""));
        assert!(prompt.contains("browser.read"));
    }
    #[test]
    fn browser_read_and_scroll_are_observe_class_methods() {
        assert_eq!(effective_effect("browser.read", "observe"), "observe");
        assert_eq!(effective_effect("browser.scroll", "observe"), "observe");
        assert_eq!(effective_effect("browser.find", "observe"), "observe");
        assert_eq!(effective_effect("browser.wait", "observe"), "observe");
        assert!(ALLOWED_PLAN_METHODS.contains(&"browser.read"));
        assert!(ALLOWED_PLAN_METHODS.contains(&"browser.scroll"));
        assert!(ALLOWED_PLAN_METHODS.contains(&"browser.find"));
        assert!(ALLOWED_PLAN_METHODS.contains(&"browser.wait"));
    }
    #[test]
    fn browser_skill_attaches_for_portal_goals() {
        assert!(goal_needs_browser_skill(
            "Open our Datadog portal and analyse RUM errors",
            &[]
        ));
        assert!(goal_needs_browser_skill(
            "anything",
            &["Installed browser".to_string()]
        ));
        assert!(!goal_needs_browser_skill(
            "Open Notepad and type hello",
            &[]
        ));
        let prompt = build_planner_prompt(
            "Open https://app.datadoghq.com and read RUM errors",
            &["Installed browser".to_string()],
            "Installed browser:\npage: https://app.datadoghq.com",
            &[],
            &[],
            true,
        );
        assert!(prompt.contains("BROWSER SKILL"));
        assert!(prompt.contains("browser.find"));
        assert!(prompt.contains("browser.wait"));
        assert!(prompt.contains("Never invent portal data"));
    }
    #[test]
    fn planner_digest_keeps_read_paging_metadata() {
        let read = serde_json::json!({
            "ok": true,
            "result": {
                "text": "Error rate spiked to 4.2%",
                "hasMore": true,
                "nextOffset": 6000
            }
        });
        let digest = planner_result_digest(&read);
        assert!(digest.contains("Error rate spiked"));
        assert!(digest.contains("hasMore"));
        assert!(digest.contains("6000"));
    }
    #[test]
    fn planner_prompt_carries_goal_observations_and_rules() {
        let prompt = build_planner_prompt(
            "Copy the total into Notepad",
            &["Installed browser".to_string(), "Notepad".to_string()],
            "Installed browser:\npage: https://example.test",
            &["browser.navigate — ok".to_string()],
            &[],
            true,
        );
        assert!(prompt.contains("Copy the total into Notepad"));
        assert!(prompt.contains("https://example.test"));
        assert!(prompt.contains("browser.navigate — ok"));
        assert!(prompt.contains("NEVER propose deletion"));
        assert!(prompt.contains("Never include processId"));
    }
    #[test]
    fn native_x_goal_never_advertises_extension_only_actions() {
        let prompt = build_planner_prompt(
            "go to x.com and post a fun fact as tweet",
            &["Microsoft Edge".to_string()],
            "Microsoft Edge: unavailable (not open)",
            &[],
            &[],
            false,
        );
        assert!(prompt.contains("NATIVE VISUAL MODE"));
        assert!(prompt.contains("https://x.com/compose/post"));
        assert!(prompt.contains("navigateApplication {\"url\":\"https://...\"}"));
        assert!(prompt.contains("`browser.*` methods"));
        assert!(prompt.contains("are unavailable"));
        let methods = prompt
            .split("Methods available on this turn:")
            .nth(1)
            .unwrap()
            .split("Rules:")
            .next()
            .unwrap();
        assert!(!methods.contains("browser.navigate"));
        assert!(!methods.contains("Installed browser"));
    }
    #[test]
    fn native_navigation_requires_allowlisted_browser_and_http_url() {
        assert!(is_native_browser_application("Microsoft Edge"));
        assert!(is_native_browser_application("Google Chrome"));
        assert!(!is_native_browser_application("PowerShell"));
        assert!(is_safe_http_url("https://x.com/compose/post"));
        assert!(is_safe_http_url("http://localhost:1420"));
        assert!(!is_safe_http_url("file:///C:/Windows/System32/cmd.exe"));
        assert!(!is_safe_http_url("javascript:alert(1)"));
        assert!(!is_safe_http_url("https://user:secret@example.com"));
        assert!(!is_safe_http_url("https:///missing-host"));
        let file_nav = WorkflowStep {
            id: "nav".into(),
            title: "Open a file URL".into(),
            kind: "browser.navigate".into(),
            effect: "external_write".into(),
            application: Some("Installed browser".into()),
            intent: Some("navigate".into()),
            target_label: Some("Address".into()),
            payload: Some(serde_json::json!({"url": "file:///C:/Windows/System32/cmd.exe"})),
            timeout_ms: default_timeout(),
            retries: default_retries(),
            wait_for: None,
            expect: None,
            save_as: None,
        };
        assert!(validate_workflow_step(&file_nav)
            .unwrap_err()
            .contains("absolute HTTP(S) URL"));
    }

    #[test]
    fn publication_proof_needs_an_authored_anchor() {
        assert!(requires_published_text_proof(
            "send an email",
            Some("Hello from Alfred"),
            false
        ));
        assert!(
            !requires_published_text_proof("send an email", None, true),
            "a publish click without typed text must not demand an absent string"
        );
        assert!(!requires_published_text_proof(
            "summarize this dashboard",
            None,
            false
        ));
        assert!(authored_text_anchor("Hello from Alfred").is_some());
        assert!(authored_text_anchor("Subject: lunch tomorrow").is_some());
        assert!(authored_text_anchor("C:\\Users\\me\\Desktop\\notes.txt").is_none());
        assert!(authored_text_anchor("notes.txt").is_none());
        assert!(authored_text_anchor("https://example.com").is_none());
        let reply = parse_planner_action(
            r#"{"done":true,"verified":true,"summary":"Email sent.","evidence":["Outlook title shows Sent Items"]}"#,
        )
        .unwrap();
        let sent = "Microsoft Outlook  gen=3\ntitle: Sent Items";
        assert!(
            is_verified_completion(&reply, sent, None, false),
            "destination-page evidence must complete a send-the-draft goal"
        );
        let mut prompt = String::from("state");
        append_completion_review(&mut prompt, "Email sent", None, false, true);
        assert!(prompt.contains("destination outcome"));
        assert!(!prompt.contains("cannot complete"));
    }
    #[test]
    fn native_tree_summary_keeps_actionable_controls_only() {
        let tree = serde_json::json!({
            "name": "", "controlType": "ControlType.Pane",
            "children": [
                { "name": "File", "controlType": "ControlType.MenuItem", "automationId": "mFile", "children": [] },
                { "name": "", "controlType": "ControlType.Pane", "children": [
                    { "name": "Save", "controlType": "ControlType.Button", "automationId": "btnSave", "children": [] }
                ]}
            ]
        });
        let mut lines = Vec::new();
        summarize_native_tree(&tree, &mut lines, 0);
        assert!(lines
            .iter()
            .any(|line| line.contains("MenuItem") && line.contains("mFile")));
        assert!(lines
            .iter()
            .any(|line| line.contains("Button") && line.contains("btnSave")));
        assert!(!lines.iter().any(|line| line.contains("Pane")));
    }
    #[test]
    fn native_tree_summary_includes_screen_bounds_for_visual_grounding() {
        let tree = serde_json::json!({
            "name": "Post", "controlType": "ControlType.Button", "automationId": "postButton",
            "bounds": {"x": 920, "y": 640, "width": 80, "height": 32}, "children": []
        });
        let mut lines = Vec::new();
        summarize_native_tree(&tree, &mut lines, 0);
        assert_eq!(
            lines.first().map(String::as_str),
            Some("- Button \"Post\" (id: postButton) [screen x=920 y=640 w=80 h=32]")
        );
    }
}
