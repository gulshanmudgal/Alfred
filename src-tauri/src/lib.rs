use chrono::{DateTime, Datelike, Local, Timelike, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
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
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt as _;
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
    /// One-step policy overrides granted by the user at the "waiting" prompt:
    /// run_id -> step_id. Lets an approved request_user step pass once, including
    /// unknown-effect steps that no permission grant could cover. hard_deny is
    /// never overridable.
    approved_overrides: Arc<Mutex<HashMap<String, String>>>,
}

/// The host speaks newline-delimited JSON on stdio. A dedicated worker thread owns
/// the pipes so callers can time out a stuck request, kill the host, and recover
/// instead of blocking the automation runtime forever.
struct NativeHostProcess {
    child: Child,
    to_host: mpsc::Sender<String>,
    from_host: mpsc::Receiver<Result<String, String>>,
    capability_token: String,
}

fn spawn_native_host(app: &AppHandle) -> Result<NativeHostProcess, String> {
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut child = Command::new(native_host_executable(app)?)
        .env("ALFRED_CAPABILITY_TOKEN", &token)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
    let (to_host, worker_inbox) = mpsc::channel::<String>();
    let (worker_outbox, from_host) = mpsc::channel::<Result<String, String>>();
    std::thread::spawn(move || {
        while let Ok(line) = worker_inbox.recv() {
            let result = (|| {
                writeln!(stdin, "{line}").map_err(|error| error.to_string())?;
                stdin.flush().map_err(|error| error.to_string())?;
                let mut response = String::new();
                stdout
                    .read_line(&mut response)
                    .map_err(|error| error.to_string())?;
                if response.is_empty() {
                    return Err("The native host closed the connection.".to_string());
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
    })
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
    /// visual grounding is opt-in. Only providers with verified image-input
    /// support (Codex today) receive attachments; others stay text-only.
    #[serde(default)]
    share_screenshots_with_planner: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            onboarding_complete: false,
            provider: "codex".into(),
            library_path: String::new(),
            screenshot_retention: "failures".into(),
            theme: "system".into(),
            share_screenshots_with_planner: false,
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
    let mut args: Vec<String> = args.into_iter().map(str::to_string).collect();
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
    provider: &str,
    prompt: &str,
    images: &[PathBuf],
) -> Result<(tokio::process::Command, Option<String>), String> {
    let invocation = provider_invocation(provider, prompt, images)?;
    let resolved = resolve_provider_command(&invocation.command).ok_or_else(|| {
        format!(
            "{} is not available to Alfred. Install it, sign in, then restart Alfred.",
            invocation.command
        )
    })?;
    let resolved = resolved_process(&resolved, &invocation.args, provider == "codex")?;
    let mut process = tokio::process::Command::new(&resolved.program);
    process
        .args(resolved.args)
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

#[tauri::command]
async fn start_provider_run(
    app: AppHandle,
    state: State<'_, RuntimeState>,
    provider: String,
    prompt: String,
    working_directory: Option<String>,
    session_id: Option<String>,
) -> Result<String, String> {
    if prompt.trim().is_empty() {
        return Err("The provider prompt cannot be empty.".into());
    }
    let _ = working_directory;
    let (mut process, prompt_input) = provider_command(&provider, &prompt, &[])?;
    let planning_directory = app_data_dir(&app)?.join("planner");
    fs::create_dir_all(&planning_directory).map_err(|error| error.to_string())?;
    process.current_dir(planning_directory);
    let mut child = process
        .spawn()
        .map_err(|error| format!("Could not start {provider}: {error}"))?;
    if let Some(prompt_input) = prompt_input {
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
    "findElement",
    "getValue",
    "invokeElement",
    "setValue",
    "click",
    "typeText",
    "key",
    "browser.observe",
    "browser.navigate",
    "browser.click",
    "browser.type",
    "browser.getText",
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
    // Single source of truth: the same classification the policy floor uses.
    if kind_is_observe(method) {
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
                if matches!(key.as_str(), "text" | "output_text" | "content" | "result") {
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
            reason: "Alfred blocked the Delete key. Deletion keystrokes are never automated.".into(),
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
            | "browser.gettext"
            | "listapplications"
            | "resolveapplication"
            | "health"
    )
}

fn effective_effect(kind: &str, declared: &str) -> String {
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
    let value: Value = serde_json::from_str(&line).map_err(|error| error.to_string())?;
    // The bridge reports failures inside the envelope; surface them so runs fail
    // (and retry) honestly instead of treating a rejected command as success.
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Browser action failed.")
            .to_string());
    }
    Ok(value)
}

#[tauri::command]
fn send_browser_command(app: AppHandle, command: BrowserCommand) -> Result<Value, String> {
    send_browser_command_inner(app, command, false)
}

fn substitute_text(text: &str, variables: &HashMap<String, String>) -> String {
    let mut result = text.to_string();
    for (key, replacement) in variables {
        result = result.replace(&format!("${{{key}}}"), replacement);
    }
    result
}

/// Replaces `${name}` placeholders in every string of a step payload with values
/// captured by earlier steps, so data can flow from one application into another.
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
    let pending: PendingApproval = serde_json::from_str(&contents).map_err(|error| error.to_string())?;
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
        return Err("The native host is not responding; it will restart on the next action.".into());
    }
    let response = match host.from_host.recv_timeout(timeout) {
        Ok(Ok(line)) => line,
        Ok(Err(error)) => {
            let _ = host.child.kill();
            *guard = None;
            return Err(error);
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
        return Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Native action failed.")
            .to_string());
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

/// Re-resolve a recorded application name to the process that owns its window
/// right now. Recorded PIDs go stale and can even be reused by other programs,
/// so replay always re-binds identity through this lookup.
/// Per-attempt PID rebinding applies to every native step EXCEPT launches: the
/// point of launchApplication is that the application is not running yet, so
/// pre-resolving it would always fail and the launch would never happen.
fn needs_process_resolution(kind: &str, application: &str) -> bool {
    kind != "launchApplication" && application != "Alfred"
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
    execute_native_action_inner(&app, &state, request, method, Duration::from_secs(30), false)
}

fn checkpoint_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    Ok(checkpoints_dir(app)?.join(format!("{run_id}.json")))
}

fn variables_path(app: &AppHandle, run_id: &str) -> Result<PathBuf, String> {
    Ok(checkpoints_dir(app)?.join(format!("{run_id}.variables.json")))
}

fn load_variables(app: &AppHandle, run_id: &str) -> HashMap<String, String> {
    variables_path(app, run_id)
        .ok()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

fn save_variables(app: &AppHandle, run_id: &str, variables: &HashMap<String, String>) {
    if let Ok(path) = variables_path(app, run_id) {
        let _ = write_json(&path, variables);
    }
}

/// Evaluates one step condition against live application state. Native steps use a
/// UIA lookup in the resolved process; browser steps use a DOM observation of the
/// pinned (or active) tab. `${variable}` placeholders are resolved first.
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

enum WaitOutcome {
    Satisfied,
    TimedOut,
    Stopped,
}

/// Polls a condition until it holds or the deadline passes. Transient lookup
/// errors (busy app, restarting host) keep the wait alive; stop/pause from the
/// user are honored between polls.
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
    let updated = value.get("updatedAt")?.as_str()?.parse::<DateTime<Utc>>().ok()?;
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
    total: usize,
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
#[tauri::command]
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

async fn drive_workflow_run(app: AppHandle, run_id: String, workflow: Workflow, start_index: usize) {
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
                detail: "Checking permission and handing the action to the trusted host."
                    .into(),
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
            let mut payload = step.payload.clone().unwrap_or_else(|| serde_json::json!({}));
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
            let floored_effect = effective_effect(&step.kind, &step.effect);
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
                                last_error = "The action ran but the expected state did not appear in time."
                                    .into();
                                attempt += 1;
                                if attempt <= attempts {
                                    tokio::time::sleep(std::time::Duration::from_millis(500))
                                        .await;
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
                        total,
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

/// One reply from the planner: either the next action or a completion signal.
/// Deliberately mirrors the workflow-step shape so goal actions flow through the
/// same policy gate, approval parking, and executors as recorded steps.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlannerReply {
    #[serde(default)]
    done: bool,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    application: Option<String>,
    #[serde(default)]
    intent: Option<String>,
    #[serde(default)]
    effect: Option<String>,
    #[serde(default)]
    target_label: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

/// Parses one candidate text into a planner reply: direct JSON first (markdown
/// fences stripped), then the widest brace span for prose-wrapped answers.
fn planner_reply_from_text(text: &str) -> Option<PlannerReply> {
    let candidate = strip_json_fence(text);
    let accept = |reply: PlannerReply| (reply.done || reply.kind.is_some()).then_some(reply);
    if let Ok(reply) = serde_json::from_str::<PlannerReply>(candidate) {
        if let Some(reply) = accept(reply) {
            return Some(reply);
        }
    }
    if let (Some(start), Some(end)) = (candidate.find('{'), candidate.rfind('}')) {
        if start < end {
            if let Ok(reply) = serde_json::from_str::<PlannerReply>(&candidate[start..=end]) {
                if let Some(reply) = accept(reply) {
                    return Some(reply);
                }
            }
        }
    }
    None
}

fn parse_planner_action(output: &str) -> Result<PlannerReply, String> {
    // Provider CLIs wrap answers differently: bare JSON, prose around the JSON,
    // markdown fences, or JSONL event envelopes with the reply nested as a
    // string (Codex item.text; Grok/Cursor stream-json content/result blocks).
    // Try the whole output, then event lines from the end — answers come
    // last — unwrapping envelopes with the same machinery as plan extraction.
    if let Some(reply) = planner_reply_from_text(output) {
        return Ok(reply);
    }
    for line in output.lines().rev() {
        let trimmed = line.trim();
        if !(trimmed.starts_with('{') && trimmed.ends_with('}')) {
            continue;
        }
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
    Err("The planner did not return a usable action.".into())
}

const PLANNER_TURN_TIMEOUT_SECS: u64 = 180;

/// One agent-loop turn: a fresh, sandboxed provider process per turn. The loop
/// keeps the state (goal, observations, history), so sessions stay stateless and
/// each turn is independently cancellable — stopping the run drops the child,
/// and kill_on_drop terminates the CLI.
async fn run_planner_turn(
    app: &AppHandle,
    run_id: &str,
    provider: &str,
    prompt: &str,
    images: &[PathBuf],
) -> Result<String, String> {
    let (mut process, prompt_input) = provider_command(provider, prompt, images)?;
    let mut child = process
        .spawn()
        .map_err(|error| format!("Could not start {provider}: {error}"))?;
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
    let deadline = std::time::Instant::now() + Duration::from_secs(PLANNER_TURN_TIMEOUT_SECS);
    loop {
        if run_mode(app, run_id) == "stop" {
            return Err("stopped".into());
        }
        match tokio::time::timeout(Duration::from_millis(500), &mut wait).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Ok(format!("{stdout}\n{stderr}"));
            }
            Ok(Err(error)) => return Err(format!("The planner process failed: {error}")),
            Err(_) => {
                if std::time::Instant::now() >= deadline {
                    return Err("The planner did not answer within 180 seconds.".into());
                }
            }
        }
    }
}

/// Compacts a UIA observation tree into the lines a planner can act on.
fn summarize_native_tree(node: &Value, out: &mut Vec<String>, depth: usize) {
    if out.len() >= 40 || depth > 6 {
        return;
    }
    let control = node.get("controlType").and_then(Value::as_str).unwrap_or("");
    let name = node.get("name").and_then(Value::as_str).unwrap_or("");
    let automation_id = node.get("automationId").and_then(Value::as_str).unwrap_or("");
    let interesting = matches!(
        control,
        "ControlType.Button" | "ControlType.Edit" | "ControlType.MenuItem"
            | "ControlType.ListItem" | "ControlType.Hyperlink" | "ControlType.TabItem"
            | "ControlType.ComboBox" | "ControlType.CheckBox" | "ControlType.RadioButton"
            | "ControlType.Document" | "ControlType.Text" | "ControlType.Window"
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
        (&["browser", "website", "web page", "webpage"], "Installed browser"),
        (&["excel", "spreadsheet", "workbook"], "Microsoft Excel"),
        (&["ms word", "word document", "word"], "Microsoft Word"),
        (&["powerpoint", "presentation"], "Microsoft PowerPoint"),
        (&["outlook"], "Microsoft Outlook"),
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

fn planner_app_rule(applications: &[String]) -> &'static str {
    if applications.is_empty() {
        "Name the target application yourself in every action, based on the goal; browser actions use \"Installed browser\"."
    } else {
        "Use application names exactly as listed; browser actions use \"Installed browser\"."
    }
}

fn build_planner_prompt(
    goal: &str,
    applications: &[String],
    observations: &str,
    history: &[String],
) -> String {
    let history_text = if history.is_empty() {
        "(none yet)".to_string()
    } else {
        history.join("\n")
    };
    format!(
        "You are the planning brain of Alfred, a supervised desktop automation agent running on the user's machine. Propose the next single action toward the goal.\n\nGOAL: {goal}\n\nTARGET APPLICATIONS: {apps}\n\nCURRENT DESKTOP STATE:\n{observations}\n\nACTION HISTORY (oldest first):\n{history_text}\n\nReply with exactly one JSON object and nothing else (no markdown fences, no prose):\n{{\"done\": false, \"title\": \"short human label\", \"kind\": \"<method>\", \"application\": \"<exact app name>\", \"intent\": \"what and why\", \"effect\": \"observe|create|modify_reversible|external_write\", \"targetLabel\": \"<element label>\", \"payload\": {{...}}}}\nWhen the goal is fully complete, reply: {{\"done\": true, \"summary\": \"what was accomplished\"}}.\n\nMethods: listApplications (application \"Alfred\"; lists running app windows) | browser.observe | browser.navigate {{\"url\"}} | browser.click {{\"ref\"}} | browser.type {{\"ref\",\"text\"}} | browser.getText {{\"ref\"}} | launchApplication (allow-list only: Notepad, Calculator, Paint, File Explorer, Microsoft Edge, Google Chrome, Brave) | focusApplication | activate {{}} | observeWindow | findElement {{\"automationId\"|\"name\"|\"controlType\"}} | getValue (same selectors) | invokeElement (same selectors) | setValue (selectors + \"value\") | click {{\"x\",\"y\"}} | typeText {{\"text\"}} | key {{\"virtualKey\": 13|9|27}} (Enter, Tab, Escape only).\n\nRules:\n- One small action per reply; observe before acting when unsure.\n- NEVER propose deletion, trash, purge, overwrite, password entry, shell commands, or credential handling. Alfred hard-blocks them regardless of what you return.\n- Prefer setValue/invokeElement (semantic, focus-independent) over click/typeText.\n- Never include processId; Alfred injects the live one.\n- {app_rule}\n- If the last action failed or changed nothing, try a different approach instead of repeating it.\n- If a target application is not running, propose launchApplication when it is on the allow-list; otherwise reply done with a summary of the blocker.",
        apps = planner_app_list(applications),
        app_rule = planner_app_rule(applications),
    )
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
                    params,
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
                    observations.push_str(&lines.join("\n"));
                    observations.push('\n');
                }
                Err(error) => {
                    observations.push_str(&format!("Installed browser: unavailable ({error})\n"));
                }
            }
        } else if cfg!(windows) {
            let runtime = app.state::<RuntimeState>();
            let section = resolve_application_process_id(app, &runtime, application)
                .and_then(|pid| {
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
                    let mut lines = vec![format!("{application}:")];
                    summarize_native_tree(&tree, &mut lines, 0);
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
fn fail_goal_run(app: &AppHandle, run_id: &str, goal: &str, step: usize, progress: u8, error: String) {
    emit_goal_event(app, run_id, step, "Goal run failed", &error, "failed", progress);
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
const MAX_PLANNER_HISTORY: usize = 12;

/// The agent loop: observe → plan → policy-gate → execute → record, until the
/// planner declares the goal done, a guardrail trips, or the user stops the run.
/// Every action flows through the same policy engine, approval parking, run lock,
/// and targeted executors as recorded workflows — the planner only proposes.
async fn drive_goal_run(
    app: AppHandle,
    run_id: String,
    provider: String,
    goal: String,
    applications: Vec<String>,
    max_steps: u32,
    check_in_every: u32,
    share_screenshots: bool,
) {
    let lock_path = run_lock_path(&app).ok();
    let mut pinned_tab: Option<i64> = None;
    let mut history: Vec<String> = Vec::new();
    let mut consecutive_failures = 0u32;
    let mut since_check_in = 0u32;
    for step_index in 0..max_steps {
        if wait_if_paused(&app, &run_id).await {
            stop_run(&app, &run_id, &goal, step_index as usize);
            return;
        }
        if let Some(path) = &lock_path {
            write_run_lock(path, &run_id);
        }
        let progress = ((step_index * 100) / max_steps.max(1)) as u8;
        emit_goal_event(
            &app, &run_id, step_index as usize, "Observing the desktop",
            "Reading the current state of every target application.", "running", progress,
        );
        let (observations, new_pinned) =
            gather_observations(&app, &run_id, &applications, pinned_tab);
        pinned_tab = new_pinned;
        // Visual grounding: one screenshot per target app. Attached to the planner
        // turn only for CLIs with verified image input (Codex); also shown in the
        // cockpit timeline as evidence.
        let (shot_paths, shot_evidence) = if share_screenshots {
            capture_run_screenshots(&app, &run_id, &applications, step_index, pinned_tab)
        } else {
            (Vec::new(), None)
        };
        emit_goal_event(
            &app, &run_id, step_index as usize, "Planning the next action",
            &format!("{provider} is deciding the next step."), "running", progress,
        );
        let mut prompt = build_planner_prompt(&goal, &applications, &observations, &history);
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
        let output = match run_planner_turn(&app, &run_id, &provider, &prompt, flag_images).await {
            Ok(output) => output,
            Err(error) if error == "stopped" => {
                stop_run(&app, &run_id, &goal, step_index as usize);
                return;
            }
            Err(error) => {
                consecutive_failures += 1;
                history.push(format!("planner error: {error}"));
                if history.len() > MAX_PLANNER_HISTORY {
                    history.remove(0);
                }
                if consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                    fail_goal_run(&app, &run_id, &goal, step_index as usize, progress, format!("The planner is unreachable: {error}"));
                    return;
                }
                continue;
            }
        };
        let reply = match parse_planner_action(&output) {
            Ok(reply) => reply,
            Err(error) => {
                consecutive_failures += 1;
                // Show the planner what its unusable output looked like so the
                // next turn can fix the format instead of repeating it.
                let snippet: String = output.trim().chars().take(280).collect();
                history.push(format!("{error} Output began: {snippet}"));
                if history.len() > MAX_PLANNER_HISTORY {
                    history.remove(0);
                }
                if consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                    fail_goal_run(&app, &run_id, &goal, step_index as usize, progress, "The planner kept returning unusable output.".into());
                    return;
                }
                continue;
            }
        };
        if reply.done {
            let summary = reply
                .summary
                .unwrap_or_else(|| "The planner reports the goal is complete.".into());
            let _ = save_checkpoint(&app, &RunCheckpoint {
                run_id: run_id.clone(),
                workflow_id: goal.clone(),
                next_step_index: max_steps as usize,
                status: "completed".into(),
                error: None,
                updated_at: Utc::now(),
            });
            emit_goal_event(&app, &run_id, step_index as usize, "Goal completed", &summary, "completed", 100);
            return;
        }
        // 3. Execute through the same policy-gated path as recorded workflows.
        let kind = reply.kind.clone().unwrap_or_default();
        let is_browser = kind.starts_with("browser.");
        let application = reply.application.clone().unwrap_or_else(|| {
            if is_browser {
                "Installed browser".into()
            } else {
                applications.first().cloned().unwrap_or_else(|| "Alfred".into())
            }
        });
        let title = reply.title.clone().unwrap_or_else(|| kind.clone());
        let declared_effect = reply.effect.clone().unwrap_or_else(|| "unknown".into());
        let step = WorkflowStep {
            id: format!("goal-step-{step_index}"),
            title: title.clone(),
            kind: kind.clone(),
            effect: effective_effect(&kind, &declared_effect),
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
        let mut payload = step.payload.clone().unwrap_or_else(|| serde_json::json!({}));
        if !is_browser && cfg!(windows) && needs_process_resolution(&kind, &application) {
            let runtime = app.state::<RuntimeState>();
            match resolve_application_process_id(&app, &runtime, &application) {
                Ok(pid) => {
                    if let Value::Object(ref mut map) = payload {
                        map.insert("processId".into(), Value::from(pid));
                    }
                }
                Err(error) => {
                    consecutive_failures += 1;
                    history.push(format!("{title} — target unavailable: {error}"));
                    if consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
                        fail_goal_run(&app, &run_id, &goal, step_index as usize, progress, format!("Target application never became available: {error}"));
                        return;
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
        emit_goal_event(
            &app, &run_id, step_index as usize, &title,
            &format!("{kind} in {application}, checked by the safety engine."), "running", progress,
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
                        max_steps as usize,
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
                history.push(format!("{title} ({kind}) — ok"));
                if history.len() > MAX_PLANNER_HISTORY {
                    history.remove(0);
                }
                consecutive_failures = 0;
                if step.effect != "observe" {
                    since_check_in += 1;
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
                // A one-step approval override is consumed with its action.
                if let Ok(mut overrides) = app.state::<RuntimeState>().approved_overrides.lock() {
                    if overrides.get(&run_id) == Some(&step.id) {
                        overrides.remove(&run_id);
                    }
                }
                let next_progress = (((step_index + 1) * 100) / max_steps.max(1)) as u8;
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
                        next_step_index: (step_index + 1) as usize,
                        status: "running".into(),
                        error: None,
                        updated_at: Utc::now(),
                    },
                );
                // Human check-in cadence: pause so the cockpit's Resume button
                // lets the user inspect the desktop before the agent continues.
                if check_in_every > 0 && since_check_in >= check_in_every {
                    since_check_in = 0;
                    if let Ok(mut controls) = app.state::<RuntimeState>().run_controls.lock() {
                        controls.insert(run_id.clone(), "paused".into());
                    }
                    emit_goal_event(
                        &app,
                        &run_id,
                        step_index as usize,
                        "Check-in pause",
                        &format!("{check_in_every} actions completed. Review the desktop, then resume."),
                        "paused",
                        next_progress,
                    );
                }
            }
            Err(error) => {
                consecutive_failures += 1;
                history.push(format!("{title} ({kind}) — failed: {error}"));
                if history.len() > MAX_PLANNER_HISTORY {
                    history.remove(0);
                }
                emit_goal_event(
                    &app,
                    &run_id,
                    step_index as usize,
                    &title,
                    &format!("Action failed; the planner will adjust: {error}"),
                    "running",
                    progress,
                );
                if consecutive_failures >= GOAL_RUN_MAX_CONSECUTIVE_FAILURES {
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
    fail_goal_run(
        &app,
        &run_id,
        &goal,
        max_steps as usize,
        100,
        format!("Reached the step limit ({max_steps}) before the planner finished the goal."),
    );
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
    max_steps: Option<u32>,
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
    // Fail fast when the planner CLI cannot be supervised on this machine;
    // otherwise the run dies off-screen and the cockpit looks stuck.
    preflight_provider(&settings.provider)?;
    let max_steps = max_steps.unwrap_or(30).clamp(1, 100);
    let check_in_every = check_in_every.unwrap_or(0);
    let run_id = Uuid::new_v4().to_string();
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
    let share_screenshots = settings.share_screenshots_with_planner;
    let screenshot_retention = settings.screenshot_retention.clone();
    tauri::async_runtime::spawn(async move {
        drive_goal_run(
            app_for_run.clone(),
            emitted_run.clone(),
            settings.provider.clone(),
            goal,
            applications,
            max_steps,
            check_in_every,
            share_screenshots,
        )
        .await;
        let final_status = get_checkpoint(app_for_run.clone(), emitted_run.clone())
            .ok()
            .flatten()
            .map(|checkpoint| checkpoint.status)
            .unwrap_or_default();
        cleanup_run_screenshots(&app_for_run, &emitted_run, &screenshot_retention, &final_status);
        release_run_lock(&lock_path, &emitted_run);
        if let Ok(mut controls) = app_for_run.state::<RuntimeState>().run_controls.lock() {
            controls.remove(&emitted_run);
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

/// Runs do not survive an app restart, so any folder left here is residue from a
/// crashed or interrupted session.
fn sweep_stale_screenshots(app: &AppHandle) {
    if let Ok(root) = app_data_dir(app).map(|dir| dir.join("run-screenshots")) {
        let _ = fs::remove_dir_all(root);
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
                            data_url.trim_start_matches("data:image/png;base64,").to_string(),
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
                            data_url.unwrap_or_else(|| format!("data:image/png;base64,{png_base64}")),
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
            sweep_stale_screenshots(app.handle());
            start_scheduler(app.handle().clone());
            let args: Vec<String> = std::env::args().collect();
            if let Some(index) = args.iter().position(|value| value == "--run-workflow") {
                if let Some(workflow_id) = args.get(index + 1) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.hide();
                    }
                    if let Ok(settings) = get_settings(app.handle().clone()) {
                        // setup is synchronous; drive the async command to
                        // completion before wiring the exit watchdog.
                        match tauri::async_runtime::block_on(start_workflow_run(
                            app.handle().clone(),
                            settings.library_path,
                            workflow_id.clone(),
                            None,
                        )) {
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
            start_goal_run,
            set_run_control,
            approve_run_step,
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
        let invocation = provider_invocation("codex", "plan", &[]).unwrap();
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
            Some(
                "\"\"C:/Users/Test User/AppData/Roaming/npm/codex.cmd\" \"exec\" \"--json\"\""
            )
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
        let directory = std::env::temp_dir().join(format!(
            "Alfred provider wrapper {}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).unwrap();
        let script = directory.join("codex.cmd");
        fs::write(&script, "@echo off\r\necho [%~1][%~2]\r\n").unwrap();
        let resolved = windows_command_script_process(
            &script,
            &["alpha beta".into(), "gamma".into()],
            true,
        )
        .unwrap();
        let mut process = Command::new(resolved.program);
        process.args(resolved.args);
        process.raw_arg(resolved.windows_raw_argument.unwrap());
        let output = process.output().unwrap();
        let _ = fs::remove_dir_all(&directory);
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[alpha beta][gamma]");
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
            wait_for: None,
            expect: None,
            save_as: None,
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
        assert!(needs_process_resolution("typeText", "Notepad"));
        assert!(needs_process_resolution("focusApplication", "Notepad"));
        assert!(!needs_process_resolution("typeText", "Alfred"));
    }
    #[test]
    fn mutating_methods_cannot_masquerade_as_observe() {
        // A prompt-injected planner (or hand-edited YAML) declaring "observe" on a
        // mutating method must not skip the permission grant.
        assert_eq!(effective_effect("typeText", "observe"), "unknown");
        assert_eq!(effective_effect("setValue", "observe"), "unknown");
        assert_eq!(effective_effect("browser.click", "observe"), "unknown");
        assert_eq!(effective_effect("observeWindow", "observe"), "observe");
        assert_eq!(effective_effect("browser.observe", "observe"), "observe");
        assert_eq!(effective_effect("getValue", "observe"), "observe");
        assert_eq!(effective_effect("typeText", "modify_reversible"), "modify_reversible");
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
        let done = parse_planner_action("All finished.\n{\"done\": true, \"summary\": \"Saved the file.\"}").unwrap();
        assert!(done.done);
        assert_eq!(done.summary.as_deref(), Some("Saved the file."));
    }
    #[test]
    fn rejects_planner_output_without_an_action() {
        assert!(parse_planner_action("I cannot help with that.").is_err());
        assert!(parse_planner_action("{\"message\": \"no action here\"}").is_err());
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
        assert!(apps.contains(&"Installed browser".to_string()));
        assert!(apps.contains(&"Microsoft Excel".to_string()));
        assert!(infer_applications_from_goal("organize my thoughts").is_empty());
    }
    #[test]
    fn planner_prompt_guides_app_choice_when_none_listed() {
        let prompt = build_planner_prompt("Type hello somewhere safe", &[], "(no observations available)", &[]);
        assert!(prompt.contains("infer them from the goal"));
        assert!(prompt.contains("listApplications"));
    }
    #[test]
    fn planner_prompt_carries_goal_observations_and_rules() {
        let prompt = build_planner_prompt(
            "Copy the total into Notepad",
            &["Installed browser".to_string(), "Notepad".to_string()],
            "Installed browser:\npage: https://example.test",
            &["browser.navigate — ok".to_string()],
        );
        assert!(prompt.contains("Copy the total into Notepad"));
        assert!(prompt.contains("https://example.test"));
        assert!(prompt.contains("browser.navigate — ok"));
        assert!(prompt.contains("NEVER propose deletion"));
        assert!(prompt.contains("Never include processId"));
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
        assert!(lines.iter().any(|line| line.contains("MenuItem") && line.contains("mFile")));
        assert!(lines.iter().any(|line| line.contains("Button") && line.contains("btnSave")));
        assert!(!lines.iter().any(|line| line.contains("Pane")));
    }
}
