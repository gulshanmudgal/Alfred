use chrono::{DateTime, Datelike, Local, Timelike, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader as StdBufReader, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

const PROTOCOL_VERSION: &str = "1.0";
const VAULT_SERVICE: &str = "com.alfred.desktop";

#[derive(Default)]
struct RuntimeState {
    provider_pids: Arc<Mutex<HashMap<String, u32>>>,
    native_host: Arc<Mutex<Option<NativeHostProcess>>>,
    run_controls: Arc<Mutex<HashMap<String, String>>>,
}

struct NativeHostProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: StdBufReader<ChildStdout>,
    capability_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    onboarding_complete: bool,
    provider: String,
    library_path: String,
    screenshot_retention: String,
    theme: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            onboarding_complete: false,
            provider: "codex".into(),
            library_path: String::new(),
            screenshot_retention: "failures".into(),
            theme: "system".into(),
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderEvent {
    session_id: String,
    provider: String,
    stream: String,
    line: String,
    status: String,
    timestamp: DateTime<Utc>,
}

#[derive(Debug)]
struct ProviderInvocation {
    command: String,
    args: Vec<String>,
    stdin: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderPlan {
    steps: Vec<ProviderPlanStep>,
}

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

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, contents).map_err(|error| error.to_string())?;
    fs::rename(&temporary, path).map_err(|error| error.to_string())
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
        .or_else(|| paths.into_iter().find(|path| is_windows_command_script(path)))
}

fn resolve_provider_command(command: &str) -> Option<PathBuf> {
    let finder = if cfg!(target_os = "windows") {
        "where.exe"
    } else {
        "which"
    };
    let output = Command::new(finder).arg(command).output().ok()?;
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

fn resolved_process(
    path: &Path,
    args: &[String],
    allow_script_wrapper: bool,
) -> Result<(PathBuf, Vec<String>), String> {
    if cfg!(target_os = "windows") && is_windows_command_script(path) {
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
        return Ok((
            shell,
            vec!["/D".into(), "/S".into(), "/C".into(), command_line],
        ));
    }
    Ok((path.to_path_buf(), args.to_vec()))
}

fn provider_version(path: &Path) -> Option<String> {
    let args = vec!["--version".to_string()];
    let (program, args) = resolved_process(path, &args, true).ok()?;
    let output = Command::new(program).args(args).output().ok()?;
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

fn provider_invocation(provider: &str, prompt: &str) -> Result<ProviderInvocation, String> {
    let (args, stdin) = match provider {
        "codex" => (vec![
            "exec",
            "--json",
            "--ephemeral",
            "--ignore-user-config",
            "--ask-for-approval",
            "never",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "-",
        ], Some(prompt.to_string())),
        "copilot" => (vec!["-p", prompt, "-s"], None),
        "cursor" => (vec!["-p", "--output-format", "stream-json", prompt], None),
        "grok" => (vec!["-p", prompt, "--output-format", "streaming-json"], None),
        _ => return Err(format!("Unknown provider: {provider}")),
    };
    let command = provider_definitions()
        .into_iter()
        .find(|item| item.0 == provider)
        .map(|item| item.2)
        .ok_or_else(|| format!("Unknown provider: {provider}"))?;
    Ok(ProviderInvocation {
        command: command.into(),
        args: args.into_iter().map(str::to_string).collect(),
        stdin,
    })
}

#[tauri::command]
async fn start_provider_run(
    app: AppHandle,
    state: State<'_, RuntimeState>,
    provider: String,
    prompt: String,
    _working_directory: Option<String>,
    session_id: Option<String>,
) -> Result<String, String> {
    if prompt.trim().is_empty() {
        return Err("The provider prompt cannot be empty.".into());
    }
    let invocation = provider_invocation(&provider, &prompt)?;
    let resolved = resolve_provider_command(&invocation.command).ok_or_else(|| {
        format!(
            "{} is not available to Alfred. Install it, sign in, then restart Alfred.",
            invocation.command
        )
    })?;
    let (program, args) = resolved_process(&resolved, &invocation.args, provider == "codex")?;
    let mut process = tokio::process::Command::new(&program);
    process
        .args(args)
        .stdin(if invocation.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Ok(secret) = vault_entry(&provider)
        .and_then(|entry| entry.get_password().map_err(|error| error.to_string()))
    {
        match provider.as_str() {
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
    let planning_directory = app_data_dir(&app)?.join("planner");
    fs::create_dir_all(&planning_directory).map_err(|error| error.to_string())?;
    process.current_dir(planning_directory);
    let mut child = process
        .spawn()
        .map_err(|error| format!("Could not start {provider}: {error}"))?;
    if let Some(prompt_input) = invocation.stdin {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("Could not open {provider} input."))?;
        stdin
            .write_all(prompt_input.as_bytes())
            .await
            .map_err(|error| format!("Could not send the plan request to {provider}: {error}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| format!("Could not finish the plan request to {provider}: {error}"))?;
    }
    let session_id = session_id
        .filter(|value| Uuid::parse_str(value).is_ok())
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if let Some(pid) = child.id() {
        state
            .provider_pids
            .lock()
            .map_err(|_| "Provider state is unavailable")?
            .insert(session_id.clone(), pid);
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let emitted_session = session_id.clone();
    let emitted_provider = provider.clone();
    let pids = state.provider_pids.clone();
    tauri::async_runtime::spawn(async move {
        let stdout_app = app.clone();
        let stdout_session = emitted_session.clone();
        let stdout_provider = emitted_provider.clone();
        let stdout_task = tauri::async_runtime::spawn(async move {
            if let Some(stdout) = stdout {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = stdout_app.emit(
                        "alfred://provider-event",
                        ProviderEvent {
                            session_id: stdout_session.clone(),
                            provider: stdout_provider.clone(),
                            stream: "stdout".into(),
                            line,
                            status: "running".into(),
                            timestamp: Utc::now(),
                        },
                    );
                }
            }
        });
        let stderr_app = app.clone();
        let stderr_session = emitted_session.clone();
        let stderr_provider = emitted_provider.clone();
        let stderr_task = tauri::async_runtime::spawn(async move {
            if let Some(stderr) = stderr {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = stderr_app.emit(
                        "alfred://provider-event",
                        ProviderEvent {
                            session_id: stderr_session.clone(),
                            provider: stderr_provider.clone(),
                            stream: "stderr".into(),
                            line,
                            status: "running".into(),
                            timestamp: Utc::now(),
                        },
                    );
                }
            }
        });
        let status = child
            .wait()
            .await
            .map(|value| {
                if value.success() {
                    "completed"
                } else {
                    "failed"
                }
            })
            .unwrap_or("failed");
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let _ = app.emit(
            "alfred://provider-event",
            ProviderEvent {
                session_id: emitted_session.clone(),
                provider: emitted_provider,
                stream: "system".into(),
                line: format!("Provider run {status}"),
                status: status.into(),
                timestamp: Utc::now(),
            },
        );
        if let Ok(mut map) = pids.lock() {
            map.remove(&emitted_session);
        }
    });
    Ok(session_id)
}

#[tauri::command]
fn cancel_provider_run(state: State<'_, RuntimeState>, session_id: String) -> Result<(), String> {
    let pid = state
        .provider_pids
        .lock()
        .map_err(|_| "Provider state is unavailable")?
        .get(&session_id)
        .copied()
        .ok_or_else(|| "Provider session is no longer running.".to_string())?;
    let status = if cfg!(target_os = "windows") {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T"])
            .status()
    } else {
        Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
    };
    status.map_err(|error| error.to_string()).and_then(|value| {
        if value.success() {
            Ok(())
        } else {
            Err("The provider process could not be stopped.".into())
        }
    })
}

const ALLOWED_PLAN_METHODS: &[&str] = &[
    "launchApplication",
    "focusApplication",
    "observeWindow",
    "captureWindow",
    "invokeElement",
    "click",
    "typeText",
    "key",
    "browser.observe",
    "browser.navigate",
    "browser.click",
    "browser.type",
];

const SAFE_LAUNCH_APPLICATIONS: &[&str] = &[
    "Notepad",
    "Calculator",
    "Paint",
    "File Explorer",
    "Microsoft Edge",
    "Google Chrome",
    "Brave",
];

fn method_effect(method: &str) -> &'static str {
    if method.ends_with("observe") || matches!(method, "observeWindow" | "captureWindow") {
        "observe"
    } else {
        "modify_reversible"
    }
}

fn validate_workflow_step(step: &WorkflowStep) -> Result<(), String> {
    if !ALLOWED_PLAN_METHODS.contains(&step.kind.as_str()) {
        return Err(format!("Unsupported workflow method: {}", step.kind));
    }
    let application = step
        .application
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Every workflow step must name an application.".to_string())?;
    if step.kind == "launchApplication"
        && !SAFE_LAUNCH_APPLICATIONS
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(application))
    {
        return Err(format!("Alfred cannot safely launch {application}."));
    }
    let params = step
        .payload
        .as_ref()
        .ok_or_else(|| format!("Parameters for {} must be a JSON object.", step.kind))?;
    if !params.is_object() {
        return Err(format!("Parameters for {} must be a JSON object.", step.kind));
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
    if step.kind == "key"
        && params.get("virtualKey").and_then(Value::as_u64) == Some(0x2e)
    {
        return Err("The Delete key is blocked by Alfred's deletion policy.".into());
    }
    if matches!(
        step.kind.as_str(),
        "invokeElement" | "click" | "browser.click" | "browser.type"
    ) && step
        .target_label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none()
    {
        return Err(format!("{} requires a visible target label.", step.kind));
    }
    if step.effect != method_effect(&step.kind) {
        return Err(format!("The effect for {} does not match its method.", step.kind));
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

fn plan_from_json_value(value: Value) -> Option<ProviderPlan> {
    if value.is_array() {
        return serde_json::from_value::<Vec<ProviderPlanStep>>(value)
            .ok()
            .map(|steps| ProviderPlan { steps });
    }
    serde_json::from_value(value).ok()
}

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
                if matches!(key.as_str(), "text" | "output_text" | "content") {
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
                return Err(format!("The provider proposed an unsupported method: {method}"));
            }
            if application.is_empty() {
                return Err("Every provider step must name an application.".into());
            }
            if method == "launchApplication"
                && !SAFE_LAUNCH_APPLICATIONS
                    .iter()
                    .any(|allowed| allowed.eq_ignore_ascii_case(&application))
            {
                return Err(format!("Alfred cannot safely launch {application}."));
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
            let effect = method_effect(&method).to_string();
            let title = if item.title.trim().is_empty() {
                item.target_label
                    .clone()
                    .unwrap_or_else(|| method.clone())
            } else {
                item.title.trim().to_string()
            };
            let step = WorkflowStep {
                id: Uuid::new_v4().to_string(),
                title,
                kind: method.clone(),
                effect: effect.clone(),
                application: Some(application.clone()),
                intent: Some(format!("{method} {}", item.target_label.clone().unwrap_or_default()).trim().to_string()),
                target_label: item.target_label,
                payload: Some(item.params),
                timeout_ms: default_timeout(),
                retries: default_retries(),
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

#[tauri::command]
fn parse_provider_plan(output: Vec<String>) -> Result<Vec<WorkflowStep>, String> {
    parse_provider_plan_output(&output)
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

#[tauri::command]
fn create_workflow(library_path: String, name: String, goal: String) -> Result<Workflow, String> {
    if name.trim().is_empty() || goal.trim().is_empty() {
        return Err("A workflow needs both a name and a goal.".into());
    }
    let now = Utc::now();
    let workflow = Workflow {
        id: Uuid::new_v4().to_string(),
        name: name.trim().into(),
        goal: goal.trim().into(),
        version: "0.2.0".into(),
        created_at: now,
        updated_at: now,
        status: "recording".into(),
        required_apps: Vec::new(),
        steps: Vec::new(),
    };
    save_workflow(&workflow_path(&library_path, &workflow), &workflow)?;
    Ok(workflow)
}

#[tauri::command]
fn record_action(
    library_path: String,
    workflow_id: String,
    mut step: WorkflowStep,
) -> Result<Workflow, String> {
    validate_workflow_step(&step)?;
    let application = step.application.clone().unwrap_or_else(|| "Alfred".into());
    let request = ActionRequest {
        protocol_version: protocol_version(),
        run_id: "recording".into(),
        workflow_step: step.id.clone(),
        application: application.clone(),
        intent: step.intent.clone().unwrap_or_else(|| step.title.clone()),
        effect: step.effect.clone(),
        target_label: step.target_label.clone(),
        payload: step.payload.clone(),
    };
    let decision = evaluate_base_policy(&request);
    if decision.decision == "hard_deny" {
        return Err(decision.reason);
    }
    let (path, mut workflow) = load_workflow(&library_path, &workflow_id)?;
    if !workflow.required_apps.contains(&application) {
        workflow.required_apps.push(application);
    }
    if step.id.trim().is_empty() {
        step.id = Uuid::new_v4().to_string();
    }
    workflow.steps.push(step);
    workflow.updated_at = Utc::now();
    workflow.status = "recording".into();
    save_workflow(&path, &workflow)?;
    Ok(workflow)
}

#[tauri::command]
fn record_actions(
    library_path: String,
    workflow_id: String,
    mut steps: Vec<WorkflowStep>,
) -> Result<Workflow, String> {
    if steps.is_empty() {
        return Err("The approved plan is empty.".into());
    }
    for step in &steps {
        validate_workflow_step(step)?;
        let application = step.application.clone().unwrap_or_else(|| "Alfred".into());
        let decision = evaluate_base_policy(&ActionRequest {
            protocol_version: protocol_version(),
            run_id: "recording".into(),
            workflow_step: step.id.clone(),
            application,
            intent: step.intent.clone().unwrap_or_else(|| step.title.clone()),
            effect: step.effect.clone(),
            target_label: step.target_label.clone(),
            payload: step.payload.clone(),
        });
        if decision.decision == "hard_deny" {
            return Err(decision.reason);
        }
    }
    let (path, mut workflow) = load_workflow(&library_path, &workflow_id)?;
    for step in &mut steps {
        let application = step.application.clone().unwrap_or_else(|| "Alfred".into());
        if !workflow.required_apps.contains(&application) {
            workflow.required_apps.push(application);
        }
        if step.id.trim().is_empty() {
            step.id = Uuid::new_v4().to_string();
        }
    }
    workflow.steps.extend(steps);
    workflow.updated_at = Utc::now();
    workflow.status = "recording".into();
    save_workflow(&path, &workflow)?;
    Ok(workflow)
}

#[tauri::command]
fn finalize_recording(library_path: String, workflow_id: String) -> Result<Workflow, String> {
    let (path, mut workflow) = load_workflow(&library_path, &workflow_id)?;
    if workflow.steps.is_empty() {
        return Err("Record at least one action before saving the workflow.".into());
    }
    workflow.status = "ready".into();
    workflow.updated_at = Utc::now();
    workflow.version = "1.0.0".into();
    save_workflow(&path, &workflow)?;
    Ok(workflow)
}

pub fn evaluate_base_policy(request: &ActionRequest) -> ActionDecision {
    let intent = request.intent.to_lowercase();
    let effect = request.effect.to_lowercase();
    let target = request
        .target_label
        .clone()
        .unwrap_or_default()
        .to_lowercase();
    let payload = request
        .payload
        .as_ref()
        .map(Value::to_string)
        .unwrap_or_default()
        .to_lowercase();
    let destructive_terms = [
        "delete",
        "remove",
        "erase",
        "trash",
        "empty trash",
        "purge",
        "destroy",
        "shift+delete",
        "format drive",
        "revoke access",
        "clear history",
        "overwrite existing",
        "replace file",
        "drop table",
    ];
    let destructive = effect == "destructive"
        || destructive_terms
            .iter()
            .any(|term| intent.contains(term) || target.contains(term) || payload.contains(term));
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
    if request.effect == "observe" {
        return Ok(base);
    }
    let authorized = read_permissions(&app)?.into_iter().any(|grant| {
        grant.enabled
            && grant.application.eq_ignore_ascii_case(&request.application)
            && grant
                .allowed_effects
                .iter()
                .any(|effect| effect == &request.effect)
            && (grant.allowed_intents.is_empty()
                || grant.allowed_intents.iter().any(|intent| {
                    request
                        .intent
                        .to_lowercase()
                        .contains(&intent.to_lowercase())
                }))
    });
    if authorized {
        Ok(ActionDecision {
            decision: "allow".into(),
            reason: "The action matches an enabled application permission.".into(),
            rule: "explicit-application-permission".into(),
        })
    } else {
        Ok(ActionDecision {
            decision: "request_user".into(),
            reason: format!(
                "{} has not been allowed to perform this kind of action.",
                request.application
            ),
            rule: "permission-required".into(),
        })
    }
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

#[tauri::command]
fn send_browser_command(app: AppHandle, command: BrowserCommand) -> Result<Value, String> {
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
    if decision.decision != "allow" {
        return Err(format!("{}: {}", decision.decision, decision.reason));
    }
    let token = fs::read_to_string(browser_token_path(&app)?)
        .map_err(|_| "The browser bridge has not been paired yet.".to_string())?;
    let mut request = command.params.as_object().cloned().unwrap_or_default();
    request.insert("id".into(), Value::String(command.id));
    request.insert("method".into(), Value::String(command.method));
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
    serde_json::from_str(&line).map_err(|error| error.to_string())
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
) -> Result<Value, String> {
    let decision = evaluate_action(app.clone(), request.clone())?;
    if decision.decision != "allow" {
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
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let mut child = Command::new(native_host_executable(app)?)
            .env("ALFRED_CAPABILITY_TOKEN", &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| error.to_string())?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Could not open native-host input.".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Could not open native-host output.".to_string())?;
        *guard = Some(NativeHostProcess {
            child,
            stdin,
            stdout: StdBufReader::new(stdout),
            capability_token: token,
        });
    }
    let host = guard
        .as_mut()
        .ok_or_else(|| "Native host failed to start.".to_string())?;
    let message = serde_json::json!({
        "id": if request.workflow_step.is_empty() { Uuid::new_v4().to_string() } else { request.workflow_step.clone() },
        "method": method, "capabilityToken": host.capability_token, "params": request.payload.clone().unwrap_or_else(|| serde_json::json!({})),
        "application": request.application, "intent": request.intent, "target": request.target_label
    });
    writeln!(host.stdin, "{}", message).map_err(|error| error.to_string())?;
    host.stdin.flush().map_err(|error| error.to_string())?;
    let mut response = String::new();
    host.stdout
        .read_line(&mut response)
        .map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_str(&response).map_err(|error| error.to_string())?;
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Native action failed.")
            .to_string());
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

#[tauri::command]
fn execute_native_action(
    app: AppHandle,
    state: State<'_, RuntimeState>,
    request: ActionRequest,
    method: String,
) -> Result<Value, String> {
    execute_native_action_inner(&app, &state, request, method)
}

fn checkpoint_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    Ok(checkpoints_dir(app)?.join(format!("{run_id}.json")))
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

#[tauri::command]
fn start_workflow_run(
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
    let emitted_run = run_id.clone();
    let app_for_run = app.clone();
    app.state::<RuntimeState>()
        .run_controls
        .lock()
        .map_err(|_| "Run control state is unavailable.")?
        .insert(run_id.clone(), "running".into());
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
    )?;
    tauri::async_runtime::spawn(async move {
        let total = workflow.steps.len().max(1);
        for (index, step) in workflow.steps.iter().enumerate().skip(start_index) {
            loop {
                let mode = app_for_run
                    .state::<RuntimeState>()
                    .run_controls
                    .lock()
                    .ok()
                    .and_then(|map| map.get(&emitted_run).cloned())
                    .unwrap_or_else(|| "stop".into());
                if mode == "stop" {
                    let _ = save_checkpoint(
                        &app_for_run,
                        &RunCheckpoint {
                            run_id: emitted_run.clone(),
                            workflow_id: workflow.id.clone(),
                            next_step_index: index,
                            status: "stopped".into(),
                            error: None,
                            updated_at: Utc::now(),
                        },
                    );
                    return;
                }
                if mode != "paused" {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            let application = step.application.clone().unwrap_or_else(|| "Alfred".into());
            let request = ActionRequest {
                protocol_version: protocol_version(),
                run_id: emitted_run.clone(),
                workflow_step: step.id.clone(),
                application: application.clone(),
                intent: step.intent.clone().unwrap_or_else(|| step.kind.clone()),
                effect: step.effect.clone(),
                target_label: step.target_label.clone(),
                payload: step.payload.clone(),
            };
            let _ = app_for_run.emit(
                "alfred://run-event",
                RunEvent {
                    run_id: emitted_run.clone(),
                    sequence: index,
                    step_id: step.id.clone(),
                    title: step.title.clone(),
                    detail: "Checking permission and handing the action to the trusted host."
                        .into(),
                    application: application.clone(),
                    status: "running".into(),
                    progress: (index * 100 / total) as u8,
                    evidence_data_url: None,
                    timestamp: Utc::now(),
                },
            );
            let result = if step.kind.starts_with("browser.") {
                let params = step
                    .payload
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({}));
                send_browser_command(
                    app_for_run.clone(),
                    BrowserCommand {
                        id: step.id.clone(),
                        method: step.kind.trim_start_matches("browser.").into(),
                        effect: step.effect.clone(),
                        intent: step.intent.clone().unwrap_or_else(|| step.title.clone()),
                        target_label: step.target_label.clone(),
                        params,
                    },
                )
            } else {
                let runtime = app_for_run.state::<RuntimeState>();
                execute_native_action_inner(&app_for_run, &runtime, request, step.kind.clone())
            };
            let (status, detail, evidence_data_url) = match result {
                Ok(value) => {
                    let direct = value
                        .get("base64")
                        .and_then(Value::as_str)
                        .map(|data| format!("data:image/png;base64,{data}"));
                    let nested = value
                        .get("result")
                        .and_then(|item| item.get("dataUrl"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    (
                        "completed",
                        "Action completed and checkpoint saved.".to_string(),
                        direct.or(nested),
                    )
                }
                Err(error) => {
                    let _ = app_for_run.emit(
                        "alfred://run-event",
                        RunEvent {
                            run_id: emitted_run.clone(),
                            sequence: index,
                            step_id: step.id.clone(),
                            title: step.title.clone(),
                            detail: error.clone(),
                            application: application.clone(),
                            status: "failed".into(),
                            progress: (index * 100 / total) as u8,
                            evidence_data_url: None,
                            timestamp: Utc::now(),
                        },
                    );
                    let _ = save_checkpoint(
                        &app_for_run,
                        &RunCheckpoint {
                            run_id: emitted_run.clone(),
                            workflow_id: workflow.id.clone(),
                            next_step_index: index,
                            status: "failed".into(),
                            error: Some(error),
                            updated_at: Utc::now(),
                        },
                    );
                    return;
                }
            };
            let _ = app_for_run.emit(
                "alfred://run-event",
                RunEvent {
                    run_id: emitted_run.clone(),
                    sequence: index,
                    step_id: step.id.clone(),
                    title: step.title.clone(),
                    detail,
                    application,
                    status: status.into(),
                    progress: (((index + 1) * 100) / total) as u8,
                    evidence_data_url,
                    timestamp: Utc::now(),
                },
            );
            let _ = save_checkpoint(
                &app_for_run,
                &RunCheckpoint {
                    run_id: emitted_run.clone(),
                    workflow_id: workflow.id.clone(),
                    next_step_index: index + 1,
                    status: "running".into(),
                    error: None,
                    updated_at: Utc::now(),
                },
            );
        }
        let _ = save_checkpoint(
            &app_for_run,
            &RunCheckpoint {
                run_id: emitted_run.clone(),
                workflow_id: workflow.id,
                next_step_index: workflow.steps.len(),
                status: "completed".into(),
                error: None,
                updated_at: Utc::now(),
            },
        );
        if let Ok(mut controls) = app_for_run.state::<RuntimeState>().run_controls.lock() {
            controls.remove(&emitted_run);
        }
    });
    Ok(run_id)
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
    let status = Command::new("schtasks")
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
                    let _ = start_workflow_run(
                        app.clone(),
                        settings.library_path.clone(),
                        schedule.workflow_id,
                        None,
                    );
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
            start_scheduler(app.handle().clone());
            let args: Vec<String> = std::env::args().collect();
            if let Some(index) = args.iter().position(|value| value == "--run-workflow") {
                if let Some(workflow_id) = args.get(index + 1) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.hide();
                    }
                    if let Ok(settings) = get_settings(app.handle().clone()) {
                        if let Ok(run_id) = start_workflow_run(
                            app.handle().clone(),
                            settings.library_path,
                            workflow_id.clone(),
                            None,
                        ) {
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
            start_provider_run,
            cancel_provider_run,
            parse_provider_plan,
            list_workflows,
            create_workflow,
            record_action,
            record_actions,
            finalize_recording,
            list_permissions,
            grant_permission,
            set_permission_enabled,
            evaluate_action,
            get_checkpoint,
            start_workflow_run,
            set_run_control,
            list_schedules,
            save_schedule,
            set_schedule_enabled,
            browser_bridge_status,
            send_browser_command,
            execute_native_action,
            start_demo_run,
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
    fn provider_commands_are_restricted() {
        let invocation = provider_invocation("codex", "plan").unwrap();
        assert!(invocation.args.contains(&"read-only".to_string()));
        assert!(invocation
            .args
            .contains(&"--skip-git-repo-check".to_string()));
        assert!(invocation.args.contains(&"--ignore-user-config".to_string()));
        assert!(invocation.args.contains(&"never".to_string()));
        assert_eq!(invocation.args.last().map(String::as_str), Some("-"));
        assert_eq!(invocation.stdin.as_deref(), Some("plan"));
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
    fn rejects_edited_unsafe_launch_and_delete_key() {
        let unsafe_launch = WorkflowStep {
            id: "one".into(),
            title: "Open PowerShell".into(),
            kind: "launchApplication".into(),
            effect: "modify_reversible".into(),
            application: Some("PowerShell".into()),
            intent: Some("launch application".into()),
            target_label: Some("PowerShell".into()),
            payload: Some(serde_json::json!({})),
            timeout_ms: default_timeout(),
            retries: default_retries(),
        };
        assert!(validate_workflow_step(&unsafe_launch)
            .unwrap_err()
            .contains("cannot safely launch"));

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
        };
        assert!(validate_workflow_step(&delete_key)
            .unwrap_err()
            .contains("Delete key"));
    }
}
