import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { AlfredLogo } from "./AlfredLogo";
import { Icon } from "./icons";
import { ProviderMark, TRADEMARK_NOTICE, providerOwner } from "./providers";
import type { AppSettings, ProviderEffortOption, ProviderModelCatalog, ProviderStatus, RunCheckpoint, RunEvent, SystemInfo, View, Workflow, WorkflowSchedule, WorkflowStep } from "./types";

const MAX_COCKPIT_EVENTS = 80;

function appendRunEvent(current: RunEvent[], payload: RunEvent) {
  const next = [...current, payload];
  if (next.length <= MAX_COCKPIT_EVENTS) return next;
  return next.slice(next.length - MAX_COCKPIT_EVENTS).map((event, index, kept) => (
    index < kept.length - 1 && event.evidenceDataUrl
      ? { ...event, evidenceDataUrl: undefined }
      : event
  ));
}

function relativeDate(date: string) {
  const minutes = Math.max(1, Math.round((Date.now() - new Date(date).getTime()) / 60000));
  if (minutes < 60) return `${minutes}m ago`;
  if (minutes < 1440) return `${Math.round(minutes / 60)}h ago`;
  return `${Math.round(minutes / 1440)}d ago`;
}

function providerName(providers: ProviderStatus[], id: string) {
  return providers.find((item) => item.id === id)?.name ?? id;
}

function withPlannerMaps(settings: AppSettings): AppSettings {
  return {
    ...settings,
    plannerModels: settings.plannerModels ?? {},
    plannerEfforts: settings.plannerEfforts ?? {},
  };
}

function plannerChoice(settings: AppSettings, provider: string, kind: "model" | "effort") {
  const map = kind === "model" ? settings.plannerModels : settings.plannerEfforts;
  return map?.[provider] ?? "";
}

function setPlannerChoice(settings: AppSettings, provider: string, kind: "model" | "effort", value: string): AppSettings {
  const key = kind === "model" ? "plannerModels" : "plannerEfforts";
  return {
    ...settings,
    [key]: { ...(settings[key] ?? {}), [provider]: value },
  };
}

function effortsForModel(catalog: ProviderModelCatalog, modelId: string): ProviderEffortOption[] {
  const resolved = modelId || catalog.defaultModel || "";
  const model = catalog.models.find((item) => item.id === resolved);
  if (model?.efforts.length) return model.efforts;
  if (model && catalog.efforts.length === 0) return [];
  return catalog.efforts;
}

function plannerSummary(settings: AppSettings, provider: string) {
  return [plannerChoice(settings, provider, "model"), plannerChoice(settings, provider, "effort")]
    .filter(Boolean)
    .join(" · ");
}

function isArchivedWorkflow(workflow: Workflow) {
  return workflow.status.toLowerCase() === "archived";
}

function isReplayProbeStep(step: WorkflowStep) {
  if (step.saveAs?.trim()) return false;
  return [
    "observeWindow",
    "captureWindow",
    "findElement",
    "listApplications",
    "listInstalledApplications",
    "probe",
    "getValue",
    "browser.observe",
    "browser.read",
    "browser.find",
    "browser.getText",
    "browser.captureVisible",
  ].includes(step.kind);
}

// Mirrors backend workflow_can_replay: a run is a replay only when the
// workflow holds at least one valid non-probe recorded action. Keep in sync
// with is_replay_probe_step / validate_workflow_step in src-tauri/src/lib.rs.
function workflowCanReplay(workflow: Workflow) {
  if (["archived", "example", "recording"].includes(workflow.status.toLowerCase())) return false;
  return workflow.steps.some((step) => !isReplayProbeStep(step) && isValidWorkflowStep(step));
}

function isValidWorkflowStep(step: WorkflowStep) {
  if (!step.application?.trim()) return false;
  // Rust treats a missing payload as {} (params defaults to the empty object),
  // so only a non-object payload is invalid here.
  if (step.payload !== undefined && (typeof step.payload !== "object" || Array.isArray(step.payload))) return false;
  const payload = step.payload ?? {};
  if (step.kind === "typeText" && !String(payload.text ?? "").trim()) return false;
  if (step.kind === "wait" && !String(payload.text ?? "").trim() && !step.targetLabel?.trim()) return false;
  return true;
}

function OverflowMenu({ label, children }: { label: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onPointer = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);
  return (
    <div className="overflow-menu" ref={root}>
      <button
        type="button"
        className="overflow-trigger"
        aria-label={label}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <Icon name="more" size={16} />
      </button>
      {open && (
        <div className="overflow-panel" role="menu" onClick={() => setOpen(false)}>
          {children}
        </div>
      )}
    </div>
  );
}

function App() {
  const [loading, setLoading] = useState(true);
  const [system, setSystem] = useState<SystemInfo | null>(null);
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [providers, setProviders] = useState<ProviderStatus[]>([]);
  const [workflows, setWorkflows] = useState<Workflow[]>([]);
  const [view, setView] = useState<View>("home");
  const [activeWorkflow, setActiveWorkflow] = useState<Workflow | null>(null);

  const refreshWorkflows = useCallback(async (libraryPath: string) => {
    const result = await invoke<Workflow[]>("list_workflows", { libraryPath });
    setWorkflows(result.filter((workflow) => workflow.status !== "recording"));
  }, []);

  useEffect(() => {
    Promise.all([
      invoke<SystemInfo>("get_system_info"),
      invoke<AppSettings>("get_settings"),
      invoke<ProviderStatus[]>("detect_providers"),
    ])
      .then(async ([systemResult, settingsResult, providerResult]) => {
        setSystem(systemResult);
        setSettings(withPlannerMaps(settingsResult));
        setProviders(providerResult);
        if (settingsResult.onboardingComplete) await refreshWorkflows(settingsResult.libraryPath);
      })
      .finally(() => setLoading(false));
  }, [refreshWorkflows]);

  if (loading || !settings || !system) return <LoadingScreen />;

  if (!settings.onboardingComplete) {
    return (
      <Onboarding
        initialSettings={settings}
        system={system}
        providers={providers}
        onComplete={async (saved) => {
          setSettings(saved);
          await refreshWorkflows(saved.libraryPath);
        }}
      />
    );
  }

  return (
    <div className="app-shell">
      <Sidebar view={view} onView={setView} />
      <main className="main-stage">
        <TopBar
          provider={activeWorkflow?.plannerProvider ?? settings.provider}
          providers={providers}
          settings={settings}
          onOpenSettings={() => { setActiveWorkflow(null); setView("settings"); }}
        />
        {activeWorkflow ? (
          <ExecutionCockpit
            workflow={activeWorkflow}
            settings={settings}
            onWorkflowChanged={async () => refreshWorkflows(settings.libraryPath)}
            onClose={() => setActiveWorkflow(null)}
          />
        ) : (
          <>
            {view === "home" && (
              <Home
                workflows={workflows}
                providers={providers}
                settings={settings}
                onRun={setActiveWorkflow}
                onOpenSettings={() => setView("settings")}
              />
            )}
            {view === "library" && (
              <Library
                workflows={workflows}
                libraryPath={settings.libraryPath}
                onStartGoal={() => setView("home")}
                onRun={setActiveWorkflow}
                onChanged={() => refreshWorkflows(settings.libraryPath)}
              />
            )}
            {view === "settings" && (
              <SettingsView
                settings={settings}
                providers={providers}
                system={system}
                onSave={async (next) => {
                  const saved = await invoke<AppSettings>("save_settings", { settings: next });
                  setSettings(withPlannerMaps(saved));
                  if (saved.onboardingComplete) await refreshWorkflows(saved.libraryPath);
                }}
              />
            )}
          </>
        )}
      </main>
    </div>
  );
}

function LoadingScreen() {
  return (
    <div className="loading-screen">
      <div className="alfred-mark large"><AlfredLogo size={56} /></div>
      <div className="loading-dots"><span /><span /><span /></div>
    </div>
  );
}

function Sidebar({ view, onView }: { view: View; onView: (view: View) => void }) {
  const items: { id: View; label: string; icon: string }[] = [
    { id: "home", label: "Home", icon: "home" },
    { id: "library", label: "Library", icon: "workflow" },
  ];
  return (
    <aside className="sidebar">
      <div className="brand"><div className="alfred-mark"><AlfredLogo size={28} /></div><span>Alfred</span></div>
      <button className="new-goal" onClick={() => onView("home")}><Icon name="plus" size={16} /> New goal</button>
      <nav>
        {items.map((item) => (
          <button key={item.id} className={view === item.id ? "nav-item active" : "nav-item"} onClick={() => onView(item.id)}>
            <Icon name={item.icon} size={16} tiled /><span>{item.label}</span>
          </button>
        ))}
      </nav>
      <div className="sidebar-spacer" />
      <p className="safety-note">Deletion is always blocked.</p>
      <button className={view === "settings" ? "nav-item active" : "nav-item"} onClick={() => onView("settings")}>
        <Icon name="settings" size={16} tiled /><span>Settings</span>
      </button>
    </aside>
  );
}

function TopBar({ provider, providers, settings, onOpenSettings }: { provider: string; providers: ProviderStatus[]; settings: AppSettings; onOpenSettings: () => void }) {
  const current = providers.find((item) => item.id === provider);
  const summary = plannerSummary(settings, provider);
  return (
    <header className="topbar">
      <button className={current?.installed ? "brain-chip" : "brain-chip missing"} onClick={onOpenSettings}>
        <Icon name="brain" size={14} tiled />
        <i />
        {current ? current.name : providerName(providers, provider)}
        {summary ? <em>{summary}</em> : null}
      </button>
    </header>
  );
}

function Home({
  workflows,
  providers,
  settings,
  onRun,
  onOpenSettings,
}: {
  workflows: Workflow[];
  providers: ProviderStatus[];
  settings: AppSettings;
  onRun: (workflow: Workflow) => void;
  onOpenSettings: () => void;
}) {
  const visible = workflows.filter((workflow) => !isArchivedWorkflow(workflow)).slice(0, 4);
  return (
    <div className="page home-page">
      <section className="hero">
        <h1>What should Alfred do?</h1>
        <p>Describe the outcome. Alfred plans it, then works through the apps on this PC.</p>
      </section>
      <GoalLauncher providers={providers} settings={settings} onRun={onRun} onOpenSettings={onOpenSettings} />
      {visible.length > 0 && (
        <section className="section-block">
          <h2>Saved recently</h2>
          <p>Successful runs you kept for later.</p>
          <div className="workflow-grid">
            {visible.map((workflow) => <WorkflowCard key={workflow.id} workflow={workflow} onRun={() => onRun(workflow)} />)}
          </div>
        </section>
      )}
    </div>
  );
}

function GoalLauncher({
  providers,
  settings,
  onRun,
  onOpenSettings,
}: {
  providers: ProviderStatus[];
  settings: AppSettings;
  onRun: (workflow: Workflow) => void;
  onOpenSettings: () => void;
}) {
  const [goal, setGoal] = useState("");
  const brain = providers.find((item) => item.id === settings.provider);
  const brainReady = brain?.installed ?? false;
  const summary = plannerSummary(settings, settings.provider);
  const start = () => {
    if (!goal.trim() || !brainReady) return;
    const now = new Date().toISOString();
    onRun({
      id: `goal-${crypto.randomUUID()}`,
      name: goal.trim().length > 48 ? `${goal.trim().slice(0, 48)}…` : goal.trim(),
      goal: goal.trim(),
      version: "1.0.0",
      createdAt: now,
      updatedAt: now,
      status: "goal",
      plannerProvider: settings.provider,
      requiredApps: [],
      steps: [],
    });
  };
  return (
    <section className="platen">
      <label htmlFor="goal-input">Write it as you would tell a person</label>
      <textarea
        id="goal-input"
        value={goal}
        onChange={(event) => setGoal(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            start();
          }
        }}
        placeholder="Copy the invoice total from the open Edge page into Notepad"
      />
      <div className="platen-bar">
        {brainReady ? (
          <small>Uses {brain?.name}{summary ? ` · ${summary}` : ""}. Change the brain in Settings.</small>
        ) : (
          <small>No brain is ready. Choose one in Settings first.</small>
        )}
        {brainReady ? (
          <button className="primary-button" disabled={!goal.trim()} onClick={start}>
            Run <Icon name="arrow" size={16} />
          </button>
        ) : (
          <button className="primary-button" onClick={onOpenSettings}>Open Settings</button>
        )}
      </div>
    </section>
  );
}

function WorkflowCard({
  workflow,
  onRun,
  onArchive,
  onRestore,
}: {
  workflow: Workflow;
  onRun?: () => void;
  onArchive?: () => void;
  onRestore?: () => void;
}) {
  const archived = isArchivedWorkflow(workflow);
  return (
    <article className={`workflow-card${archived ? " archived" : ""}`}>
      <div className="workflow-title-row">
        <h3>{workflow.name}</h3>
        <span className={`status-badge ${workflow.status}`}>{workflow.status}</span>
        {onArchive && !archived && (
          <OverflowMenu label={`More actions for ${workflow.name}`}>
            <button type="button" role="menuitem" onClick={onArchive}>
              <Icon name="archive" size={14} /> Archive
            </button>
          </OverflowMenu>
        )}
      </div>
      <p>{workflow.goal}</p>
      <div className="app-chips">
        {(workflow.requiredApps.length ? workflow.requiredApps : ["Apps chosen while running"]).map((app) => <span key={app}>{app}</span>)}
      </div>
      <div className="workflow-card-footer">
        <span>{workflow.status === "example" ? "Safe simulation" : `Updated ${relativeDate(workflow.updatedAt)}`}</span>
        <div className="workflow-card-actions">
          {archived
            ? onRestore && <button type="button" className="ghost-button" onClick={onRestore}><Icon name="archive" size={14} /> Restore</button>
            : onRun && <button type="button" onClick={onRun}><Icon name="runs" size={14} /> Run</button>}
        </div>
      </div>
    </article>
  );
}

function Library({
  workflows,
  libraryPath,
  onStartGoal,
  onRun,
  onChanged,
}: {
  workflows: Workflow[];
  libraryPath: string;
  onStartGoal: () => void;
  onRun: (workflow: Workflow) => void;
  onChanged: () => Promise<void>;
}) {
  const [showArchived, setShowArchived] = useState(false);
  const [busyId, setBusyId] = useState("");
  const [error, setError] = useState("");
  const [scheduleRevision, setScheduleRevision] = useState(0);
  const active = workflows.filter((workflow) => !isArchivedWorkflow(workflow));
  const archived = workflows.filter(isArchivedWorkflow);
  const shown = showArchived ? archived : active;
  const setArchived = async (workflow: Workflow, next: boolean) => {
    if (busyId) return;
    setBusyId(workflow.id);
    setError("");
    try {
      await invoke("set_workflow_archived", { libraryPath, workflowId: workflow.id, archived: next });
      await onChanged();
      setScheduleRevision((value) => value + 1);
      if (!next && archived.length <= 1) setShowArchived(false);
    } catch (caught) {
      setError(String(caught));
    } finally {
      setBusyId("");
    }
  };
  return (
    <div className="page library-page">
      <section className="page-title">
        <div>
          <h1>Library</h1>
          <p>Reusable workflows and the weekday schedules that run them.</p>
        </div>
        <div className="page-title-actions library-title-actions">
          {(archived.length > 0 || showArchived) && (
            <OverflowMenu label="Library options">
              <button type="button" role="menuitem" onClick={() => setShowArchived((value) => !value)}>
                <Icon name="archive" size={14} />
                {showArchived ? "Show library" : `Show archived (${archived.length})`}
              </button>
            </OverflowMenu>
          )}
          <button className="primary-button" onClick={onStartGoal}><Icon name="plus" size={16} /> New goal</button>
        </div>
      </section>
      {error && <div className="error-message">{error}</div>}
      {shown.length === 0 ? (
        <EmptyState
          icon={showArchived ? "archive" : "folder"}
          title={showArchived ? "No archived workflows" : workflows.length ? "Nothing in the library" : "Nothing saved yet"}
          description={showArchived
            ? "Restored workflows come back here as ready."
            : workflows.length
              ? "Archived workflows stay on disk. Open the archive to restore one."
              : "Run a goal all the way through, then save it here."}
          action={showArchived || !archived.length ? "Write a goal" : "Show archived"}
          onAction={showArchived || !archived.length ? onStartGoal : () => setShowArchived(true)}
        />
      ) : (
        <div className="workflow-list">
          {shown.map((workflow) => (
            <WorkflowCard
              key={workflow.id}
              workflow={workflow}
              onRun={isArchivedWorkflow(workflow) || busyId === workflow.id ? undefined : () => onRun(workflow)}
              onArchive={busyId === workflow.id ? undefined : () => setArchived(workflow, true)}
              onRestore={busyId === workflow.id ? undefined : () => setArchived(workflow, false)}
            />
          ))}
        </div>
      )}
      <ScheduleSection workflows={active} revision={scheduleRevision} onStartGoal={onStartGoal} />
    </div>
  );
}

function ScheduleSection({ workflows, revision = 0, onStartGoal }: { workflows: Workflow[]; revision?: number; onStartGoal: () => void }) {
  const [schedules, setSchedules] = useState<WorkflowSchedule[]>([]);
  const [workflowId, setWorkflowId] = useState(workflows[0]?.id ?? "");
  const [time, setTime] = useState("09:00");
  const [error, setError] = useState("");
  const refresh = useCallback(() => invoke<WorkflowSchedule[]>("list_schedules").then(setSchedules), []);
  useEffect(() => { refresh(); }, [refresh, revision]);
  useEffect(() => {
    if (!workflows.some((item) => item.id === workflowId)) {
      setWorkflowId(workflows[0]?.id ?? "");
    }
  }, [workflows, workflowId]);
  const add = async () => {
    const workflow = workflows.find((item) => item.id === workflowId);
    if (!workflow) return;
    const [hour, minute] = time.split(":").map(Number);
    try {
      await invoke("save_schedule", { workflowId, workflowName: workflow.name, hour, minute, days: [0, 1, 2, 3, 4] });
      await refresh();
    } catch (caught) {
      setError(String(caught));
    }
  };
  return (
    <section className="section-block schedule-builder">
      <h2>Weekday schedules</h2>
      <p>On Windows these register with Task Scheduler. Elsewhere they run while Alfred is open.</p>
      {workflows.length > 0 && (
        <div className="schedule-form">
          <select value={workflowId} onChange={(event) => setWorkflowId(event.target.value)}>
            {workflows.map((item) => <option value={item.id} key={item.id}>{item.name}</option>)}
          </select>
          <input type="time" value={time} onChange={(event) => setTime(event.target.value)} />
          <button className="primary-button" disabled={!workflowId} onClick={add}>Add</button>
        </div>
      )}
      {error && <div className="error-message">{error}</div>}
      <div className="schedule-list">
        {schedules.map((schedule) => {
          const archivedSchedule = !workflows.some((item) => item.id === schedule.workflowId);
          return (
            <article className="settings-section schedule-row" key={schedule.id}>
              <div>
                <strong>{schedule.workflowName}</strong>
                <span>Weekdays at {String(schedule.hour).padStart(2, "0")}:{String(schedule.minute).padStart(2, "0")}</span>
              </div>
              <button
                type="button"
                className={schedule.enabled && !archivedSchedule ? "status-badge ready" : "status-badge"}
                disabled={archivedSchedule}
                title={archivedSchedule ? "Restore this workflow before turning its schedule back on." : undefined}
                onClick={async () => {
                  if (archivedSchedule) return;
                  try {
                    const next = await invoke<WorkflowSchedule[]>("set_schedule_enabled", { scheduleId: schedule.id, enabled: !schedule.enabled });
                    setSchedules(next);
                    setError("");
                  } catch (caught) {
                    setError(String(caught));
                  }
                }}
              >
                {archivedSchedule || !schedule.enabled ? "Off" : "On"}
              </button>
            </article>
          );
        })}
      </div>
      {!schedules.length && workflows.length === 0 && (
        <EmptyState icon="calendar" title="No schedules yet" description="Save a workflow first, then choose a weekday time." action="Write a goal" onAction={onStartGoal} />
      )}
    </section>
  );
}

function EmptyState({ icon, title, description, action, onAction }: { icon: string; title: string; description: string; action: string; onAction: () => void }) {
  return (
    <div className="empty-state">
      <div className="empty-icon"><Icon name={icon} size={22} tiled /></div>
      <h2>{title}</h2>
      <p>{description}</p>
      <button className="primary-button" onClick={onAction}>{action}</button>
    </div>
  );
}

function ExecutionCockpit({ workflow, settings, onWorkflowChanged, onClose }: { workflow: Workflow; settings: AppSettings; onWorkflowChanged: () => Promise<void>; onClose: () => void }) {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [runId, setRunId] = useState("");
  const [paused, setPaused] = useState(false);
  const [takeover, setTakeover] = useState(false);
  const [startError, setStartError] = useState("");
  const [steer, setSteer] = useState("");
  const [savingWorkflow, setSavingWorkflow] = useState(false);
  const [savedWorkflow, setSavedWorkflow] = useState<Workflow | null>(null);
  const queued = useRef<RunEvent[]>([]);
  const activeRun = useRef("");
  const early = useRef<RunEvent[]>([]);
  const pausedRef = useRef(false);
  const takeoverRef = useRef(false);
  const livePlanner = workflow.status !== "example";

  useEffect(() => {
    pausedRef.current = paused;
    takeoverRef.current = takeover;
  }, [paused, takeover]);

  useEffect(() => {
    let dispose: (() => void) | undefined;
    let active = true;
    listen<RunEvent>("alfred://run-event", ({ payload }) => {
      if (!active) return;
      if (payload.runId !== activeRun.current) {
        if (!activeRun.current) {
          early.current.push(payload);
          if (early.current.length > 80) early.current.splice(0, early.current.length - 80);
        }
        return;
      }
      if (pausedRef.current || takeoverRef.current) {
        queued.current.push(payload);
        if (queued.current.length > 80) queued.current.splice(0, queued.current.length - 80);
      } else setEvents((current) => appendRunEvent(current, payload));
    }).then((unlisten) => { if (active) dispose = unlisten; else unlisten(); });
    return () => { active = false; dispose?.(); };
  }, []);

  // Read via ref at start time: settings.provider/libraryPath only matter
  // before the run starts (they pick the planner for a goal run and locate
  // the library for a replay). Depending on them directly re-fires this
  // effect mid-run, tries to start a second run, and hits the run lock.
  const settingsRef = useRef(settings);
  settingsRef.current = settings;

  useEffect(() => {
    const current = settingsRef.current;
    if (!current) return;
    const replay = workflowCanReplay(workflow);
    const command = workflow.status === "example" ? "start_demo_run" : replay ? "start_workflow_run" : "start_goal_run";
    const args = workflow.status === "example"
      ? { workflowId: workflow.id }
      : replay
        ? { libraryPath: current.libraryPath, workflowId: workflow.id }
        : { goal: workflow.goal, applications: workflow.requiredApps, provider: workflow.plannerProvider ?? current.provider };
    invoke<string>(command, args).then((id) => {
      activeRun.current = id;
      setRunId(id);
      const held = early.current.filter((event) => event.runId === id);
      early.current = [];
      if (held.length) setEvents((current) => held.reduce(appendRunEvent, current));
    }).catch((caught) => setStartError(String(caught)));
    return () => { activeRun.current = ""; early.current = []; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workflow.id, workflow.status, workflow.goal, workflow.requiredApps, workflow.plannerProvider, workflow.steps.length]);

  useEffect(() => {
    if (!runId || startError || workflow.status === "example" || ["completed", "failed", "stopped"].includes(events.at(-1)?.status ?? "")) return;
    let cancelled = false;
    const timer = window.setInterval(() => {
      invoke<RunCheckpoint | null>("get_checkpoint", { runId }).then((checkpoint) => {
        if (cancelled || !checkpoint || checkpoint.status === "running") return;
        if (checkpoint.status === "completed") {
          setEvents((current) => current.at(-1)?.status === "completed" ? current : appendRunEvent(current, {
            runId,
            sequence: checkpoint.nextStepIndex,
            stepId: "recovered",
            title: "Run completed",
            detail: "The end-to-end run reached its completed checkpoint.",
            application: "Alfred",
            status: "completed",
            progress: 100,
            timestamp: checkpoint.updatedAt,
          }));
        } else {
          setEvents((current) => ["failed", "stopped"].includes(current.at(-1)?.status ?? "") ? current : appendRunEvent(current, {
            runId,
            sequence: checkpoint.nextStepIndex,
            stepId: "terminal",
            title: checkpoint.status === "failed" ? "Run failed" : "Run stopped",
            detail: checkpoint.error ?? "The run was stopped before completion.",
            application: "Alfred",
            status: checkpoint.status,
            progress: current.at(-1)?.progress ?? 0,
            timestamp: checkpoint.updatedAt,
          }));
        }
      }).catch(() => undefined);
    }, 3000);
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [runId, events, startError, workflow.status]);

  const resume = () => {
    setPaused(false); setTakeover(false);
    if (runId && livePlanner) invoke("set_run_control", { runId, control: "running" });
    if (queued.current.length) { setEvents((current) => queued.current.reduce(appendRunEvent, current)); queued.current = []; }
  };
  const controlRun = (control: "paused" | "stop") => {
    if (runId && livePlanner) invoke("set_run_control", { runId, control }).catch(() => undefined);
    if (control === "paused") setPaused(true); else onClose();
  };
  const approveWaitingStep = async () => {
    try { await invoke("approve_run_step", { runId }); }
    catch (caught) { setStartError(String(caught)); }
  };
  const markGoalComplete = async () => {
    try {
      const checkpoint = await invoke<RunCheckpoint>("complete_goal_run", { runId });
      setPaused(false); setTakeover(false); queued.current = [];
      setEvents((current) => current.at(-1)?.status === "completed" ? current : appendRunEvent(current, {
        runId,
        sequence: checkpoint.nextStepIndex,
        stepId: "user-completed",
        title: "Goal completed",
        detail: "You confirmed that the requested outcome is complete.",
        application: "Alfred",
        status: "completed",
        progress: 100,
        timestamp: checkpoint.updatedAt,
      }));
    } catch (caught) { setStartError(String(caught)); }
  };
  const saveAsWorkflow = async () => {
    setSavingWorkflow(true); setStartError("");
    try {
      const saved = await invoke<Workflow>("save_goal_run_as_workflow", {
        libraryPath: settings.libraryPath,
        runId,
        name: workflow.name,
        goal: workflow.goal,
      });
      setSavedWorkflow(saved);
      await onWorkflowChanged();
    } catch (caught) { setStartError(String(caught)); }
    finally { setSavingWorkflow(false); }
  };
  const sendSteer = async () => {
    const note = steer.trim();
    if (!note) return;
    setSteer("");
    let title = "You steered the run";
    if (workflow.status === "example" || !livePlanner) {
      title = "Simulations can't change course";
    } else if (!runId) {
      title = "The run is still starting — send again in a moment";
    } else {
      try { await invoke("steer_run", { runId, note: note.slice(0, 500) }); }
      catch (caught) { setStartError(String(caught)); return; }
    }
    const echo: RunEvent = { runId, sequence: 9999, stepId: `steer-${Date.now()}`, title, detail: note, application: "You", status: "running", progress: events.at(-1)?.progress ?? 3, timestamp: new Date().toISOString() };
    if (pausedRef.current || takeoverRef.current) {
      queued.current.push(echo);
      if (queued.current.length > MAX_COCKPIT_EVENTS) queued.current.splice(0, queued.current.length - MAX_COCKPIT_EVENTS);
    } else setEvents((current) => appendRunEvent(current, echo));
  };

  const current = events.at(-1);
  const progress = current?.progress ?? 3;
  const complete = progress === 100 && current?.status === "completed";
  const waitingApproval = current?.status === "waiting";
  // A replay can fall back to the live planner mid-run when a recorded step
  // no longer matches. Once planner events that don't map to recorded steps
  // appear, follow the run instead of the static recorded plan.
  const startedAsReplay = workflowCanReplay(workflow);
  const replaySteps = startedAsReplay ? workflow.steps.filter((step) => !isReplayProbeStep(step)) : [];
  const knownStepIds = new Set(replaySteps.map((step) => step.id));
  // Backend lifecycle events (goal-{sequence}, recovered/terminal/
  // user-completed/steer echoes) are not planner actions. Planner actions
  // use goal-step-{n} ids, so only the bare goal-{number} form is exempt —
  // a broader goal-* prefix would hide the takeover and keep the panel in
  // replay mode forever.
  const isLifecycleEvent = (id: string) => /^goal-\d+$/.test(id) || id.startsWith("recovered") || id.startsWith("terminal") || id.startsWith("user-completed") || id.startsWith("steer-") || id === "replay-verify";
  const plannerEvents = events.filter((event) => !knownStepIds.has(event.stepId) && !isLifecycleEvent(event.stepId));
  const fellBackToPlanner = startedAsReplay && plannerEvents.some((event) => event.status === "completed" || event.status === "running");
  const replaying = startedAsReplay && !fellBackToPlanner;
  const completedSteps = livePlanner ? events.filter((event) => event.status === "completed").slice(-8) : [];
  const planned = replaying
    ? replaySteps.map((step) => step.title)
    : livePlanner
    ? (completedSteps.length ? completedSteps.map((event) => event.title) : ["Actions appear here as they happen."])
    : workflow.steps.length ? workflow.steps.map((step) => step.title) : ["Prepare workspace", "Open approved website", "Read invoice table", "Check safety policy", "Append workbook rows", "Verify the result"];

  return (
    <div className="cockpit">
      <div className="cockpit-header">
        <div>
          <button className="back-button" onClick={onClose} aria-label="Close run">‹</button>
          <span className="run-kicker">{complete ? "COMPLETED" : waitingApproval ? "WAITING" : takeover ? "YOU HAVE CONTROL" : paused ? "PAUSED" : "RUNNING"}</span>
          <h1>{workflow.name}</h1>
        </div>
        <div className="run-controls">
          {!complete && ((paused || takeover || current?.status === "paused")
            ? <button className="secondary-button" onClick={resume}><Icon name="runs" size={16} /> Resume</button>
            : <button className="secondary-button" onClick={() => controlRun("paused")}><Icon name="pause" size={16} /> Pause</button>)}
          {!complete && <button className="secondary-button" onClick={() => { controlRun("paused"); setTakeover(true); }}><Icon name="hand" size={16} /> Take over</button>}
          {!complete && livePlanner && (paused || takeover) && <button className="primary-button" onClick={markGoalComplete}><Icon name="check" size={16} /> Mark complete</button>}
          {!complete && <button className="danger-button" onClick={() => controlRun("stop")}><Icon name="stop" size={15} /> Stop</button>}
          {complete && workflow.status === "goal" && !savedWorkflow && <button className="primary-button" disabled={savingWorkflow} onClick={saveAsWorkflow}><Icon name="workflow" size={16} /> {savingWorkflow ? "Saving…" : "Save to Library"}</button>}
          {complete && <button className="secondary-button" onClick={onClose}>Done</button>}
        </div>
      </div>
      {startError && <div className="error-message">{startError}</div>}
      {complete && (
        <div className="completion-banner panel-surface">
          <div>
            <Icon name="check" size={16} tiled />
            <div>
              <strong>This run finished</strong>
              <span>{savedWorkflow ? `Saved as ${savedWorkflow.name}.` : "Save it if you want to run the same work again."}</span>
            </div>
          </div>
          {workflow.status === "goal" && !savedWorkflow && <button className="primary-button" disabled={savingWorkflow} onClick={saveAsWorkflow}>{savingWorkflow ? "Saving…" : "Save to Library"}</button>}
        </div>
      )}
      {waitingApproval && (
        <div className="approval-banner panel-surface">
          <div>
            <strong>Alfred needs a safety exception</strong>
            <span>{current?.detail}</span>
          </div>
          <div className="run-controls">
            <button className="primary-button" onClick={approveWaitingStep}>Approve</button>
            <button className="danger-button" onClick={() => controlRun("stop")}>Deny and stop</button>
          </div>
        </div>
      )}
      <div className="progress-track"><span style={{ width: `${progress}%` }} /></div>
      <div className="cockpit-grid">
        <section className="plan-panel panel-surface">
          <div className="panel-heading"><span>Steps</span><b>{progress}%</b></div>
          <div className="plan-list">
            {planned.map((step, index) => {
              // In replay mode rows come from recorded steps and match events
              // by recorded step id. In a live goal run (including the
              // planner fallback for a non-replayable workflow) rows ARE the
              // completed events, so index directly — recorded step ids never
              // appear in a goal run's event stream.
              const recorded = replaying ? replaySteps[index] : undefined;
              const event = recorded
                ? [...events].reverse().find((item) => item.stepId === recorded.id)
                : livePlanner
                ? completedSteps[index]
                : events.find((item) => item.sequence === index);
              const done = event?.status === "completed";
              const active = event?.status === "running" || event?.status === "failed" || (!event && index === 0);
              return (
                <div key={`${step}-${index}`} className={`plan-step ${done ? "done" : active ? "current" : ""}`}>
                  <span className="step-marker">{done ? <Icon name="check" size={13} /> : index + 1}</span>
                  <div>
                    <strong>{step}</strong>
                    <small>{event ? `${event.application} · ${event.status}` : active ? "In progress" : "Waiting"}</small>
                  </div>
                </div>
              );
            })}
          </div>
          <div className="policy-lock">
            <Icon name="lock" size={14} tiled />
            <div>
              <strong>Protected</strong>
              <span>Deletion stays blocked.</span>
            </div>
          </div>
        </section>
        <section className="live-panel panel-surface">
          <div className="panel-heading">
            <span>Live</span>
            <div>
              <span className="simulation-badge">{workflow.status === "example" ? "SIMULATION" : "THIS PC"}</span>
              <i className="live-dot" /> Live
            </div>
          </div>
          <div className="screen-preview">
            {workflow.status === "example" ? (
              <>
                <div className="fake-browser-bar">
                  <span className="browser-dots"><i /><i /><i /></span>
                  <div className="fake-address">supplier.example.com/invoices</div>
                  <span>⋯</span>
                </div>
                <div className="fake-browser-content">
                  <div className="fake-app-nav"><div className="fake-logo">S</div><span>Dashboard</span><span className="selected">Invoices</span><span>Reports</span></div>
                  <div className="fake-page">
                    <div className="fake-page-title"><div><small>FINANCE</small><strong>Supplier invoices</strong></div><button>Export</button></div>
                    <div className={`fake-table ${progress >= 45 && progress < 70 ? "highlighted" : ""}`}>
                      <div className="fake-row header"><span>Invoice</span><span>Supplier</span><span>Date</span><span>Amount</span></div>
                      {["INV-1048|Northwind|Aug 7|₹24,800", "INV-1049|Contoso|Aug 7|₹18,250", "INV-1050|Fabrikam|Aug 8|₹31,400", "INV-1051|Adventure|Aug 8|₹12,990"].map((row) => <div className="fake-row" key={row}>{row.split("|").map((cell) => <span key={cell}>{cell}</span>)}</div>)}
                    </div>
                    {progress >= 45 && progress < 70 && <div className="focus-label">Alfred is reading 14 rows</div>}
                  </div>
                </div>
              </>
            ) : current?.evidenceDataUrl ? (
              <img className="native-screenshot" src={current.evidenceDataUrl} alt={`Captured ${current.application} window`} />
            ) : (
              <div className="native-preview">
                <Icon name="monitor" size={22} tiled />
                <strong>{current?.application ?? "Waiting for the Windows host"}</strong>
                <span>A screenshot appears here after a capture. Every action still shows in Activity.</span>
              </div>
            )}
            {(paused || takeover) && (
              <div className="paused-overlay">
                <div>
                  <Icon name={takeover ? "hand" : "pause"} size={20} tiled />
                  <strong>{takeover ? "You have control" : "Paused"}</strong>
                  <span>Alfred will not touch the desktop until you resume.</span>
                </div>
              </div>
            )}
          </div>
          <div className="now-doing">
            <div className="pulse-icon"><Icon name={complete ? "check" : "sparkle"} size={16} tiled /></div>
            <div>
              <small>{complete ? "DONE" : "WORKING"}</small>
              <strong>{current?.title ?? "Preparing the run"}</strong>
              <span>{current?.detail ?? "Loading run memory and looking at the desktop."}</span>
            </div>
          </div>
        </section>
        <section className="activity-panel panel-surface">
          <div className="panel-heading"><span>Activity</span></div>
          <div className="event-list">
            {events.length === 0 && <div className="event-placeholder"><div className="mini-spinner" />Waiting for the first action…</div>}
            {[...events].reverse().map((event, index) => (
              <div className="event-item" key={`${event.stepId}-${event.sequence}-${index}`}>
                <span className={index === 0 && !complete ? "event-status active" : "event-status"}>
                  <Icon name={event.status === "completed" ? "check" : "sparkle"} size={13} />
                </span>
                <div>
                  <strong>{event.title}</strong>
                  <p>{event.detail}</p>
                  <small>{new Date(event.timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })} · {event.application}</small>
                </div>
              </div>
            ))}
          </div>
          <div className="run-id">Run {runId ? runId.slice(0, 8) : "starting"} · this PC only</div>
        </section>
      </div>
      <div className="steer-bar">
        <Icon name="sparkle" size={14} tiled />
        <input
          value={steer}
          onChange={(event) => setSteer(event.target.value)}
          onKeyDown={(event) => { if (event.key === "Enter") sendSteer(); }}
          placeholder="Tell Alfred something while it works…"
        />
        <button type="button" onClick={sendSteer}>Send</button>
      </div>
    </div>
  );
}

function Onboarding({ initialSettings, system, providers, onComplete }: { initialSettings: AppSettings; system: SystemInfo; providers: ProviderStatus[]; onComplete: (settings: AppSettings) => void }) {
  const [step, setStep] = useState(0);
  const [draft, setDraft] = useState<AppSettings>({ ...withPlannerMaps(initialSettings), libraryPath: initialSettings.libraryPath || system.defaultLibraryPath });
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const steps = ["Welcome", "Brain", "Workspace"];
  const chooseFolder = async () => {
    const selected = await open({ directory: true, multiple: false, title: "Choose your Alfred workflow library" });
    if (typeof selected === "string") setDraft((current) => ({ ...current, libraryPath: selected }));
  };
  const finish = async () => {
    setSaving(true); setError("");
    try {
      const saved = await invoke<AppSettings>("save_settings", { settings: { ...draft, onboardingComplete: true } });
      onComplete(saved);
    } catch (caught) {
      setError(String(caught));
      setSaving(false);
    }
  };
  return (
    <div className="onboarding-shell">
      <aside className="onboarding-sidebar">
        <div className="brand light"><div className="alfred-mark"><AlfredLogo size={28} /></div><span>Alfred</span></div>
        <div className="onboarding-progress">
          {steps.map((title, index) => (
            <div className={`onboarding-step ${index === step ? "active" : index < step ? "done" : ""}`} key={title}>
              <span>{index < step ? <Icon name="check" size={14} /> : index + 1}</span>
              <div>
                <strong>{title}</strong>
                <small>{index === step ? "Now" : index < step ? "Done" : "Next"}</small>
              </div>
            </div>
          ))}
        </div>
        <div className="onboarding-assurance">Alfred shows its work and never deletes your files.</div>
      </aside>
      <main className="onboarding-main">
        <div className="onboarding-content">
          {step === 0 && <WelcomeStep system={system} />}
          {step === 1 && (
            <ProviderStep
              providers={providers}
              selected={draft.provider}
              model={plannerChoice(draft, draft.provider, "model")}
              effort={plannerChoice(draft, draft.provider, "effort")}
              onSelect={(provider) => setDraft({ ...draft, provider })}
              onModel={(value) => setDraft((current) => setPlannerChoice(current, current.provider, "model", value))}
              onEffort={(value) => setDraft((current) => setPlannerChoice(current, current.provider, "effort", value))}
            />
          )}
          {step === 2 && (
            <LibraryStep
              path={draft.libraryPath}
              onPath={(libraryPath) => setDraft({ ...draft, libraryPath })}
              onChoose={chooseFolder}
              retention={draft.screenshotRetention}
              onRetention={(screenshotRetention) => setDraft({ ...draft, screenshotRetention })}
            />
          )}
          {error && <div className="error-message">{error}</div>}
        </div>
        <div className="onboarding-footer">
          <button className="ghost-button" disabled={step === 0} onClick={() => setStep((current) => current - 1)}>Back</button>
          <span className="onboarding-count">{step + 1} of {steps.length}</span>
          {step < steps.length - 1 ? (
            <button className="primary-button" onClick={() => setStep((current) => current + 1)}>Continue</button>
          ) : (
            <button className="primary-button" disabled={saving} onClick={finish}>{saving ? "Saving…" : "Open Alfred"}</button>
          )}
        </div>
      </main>
    </div>
  );
}

function WelcomeStep({ system }: { system: SystemInfo }) {
  return (
    <div className="setup-step welcome-step">
      <div className="welcome-visual">
        <div className="orbit one" />
        <div className="orbit two" />
        <div className="welcome-logo"><AlfredLogo size={88} /></div>
        <span className="floating-app edge">E</span>
        <span className="floating-app excel">X</span>
        <span className="floating-app outlook">O</span>
      </div>
      <span className="eyebrow">Welcome</span>
      <h1>Tell Alfred what to do.<br />It works on this PC.</h1>
      <p>Write a goal in plain English. Alfred plans it, acts in your apps, and stops short of deleting anything. You can pause or take over at any time.</p>
      <div className="platform-chip"><Icon name="monitor" size={14} tiled />{system.os === "windows" ? "Windows" : "macOS"} · {system.architecture}</div>
    </div>
  );
}

function ProviderStep({
  providers,
  selected,
  model,
  effort,
  onSelect,
  onModel,
  onEffort,
}: {
  providers: ProviderStatus[];
  selected: string;
  model: string;
  effort: string;
  onSelect: (id: string) => void;
  onModel: (value: string) => void;
  onEffort: (value: string) => void;
}) {
  const current = providers.find((item) => item.id === selected);
  return (
    <div className="setup-step">
      <span className="eyebrow">Brain</span>
      <h1>Choose who plans the work</h1>
      <p className="setup-lead">This is the CLI Alfred asks for the next step. You can change it later in Settings — not on every goal.</p>
      <div className="provider-list">
        {providers.map((provider, index) => (
          <button key={provider.id} className={selected === provider.id ? "provider-option selected" : "provider-option"} onClick={() => onSelect(provider.id)}>
            <ProviderMark id={provider.id} />
            <div>
              <strong>{provider.name}</strong>
              <span>by {providerOwner(provider.id)} · {provider.installed ? `found as ${provider.command}` : "not installed yet"}</span>
            </div>
            {index === 0 && <em>Suggested</em>}
            <i>{selected === provider.id && <Icon name="check" size={14} />}</i>
          </button>
        ))}
      </div>
      {current && (
        <PlannerModelFields
          provider={current.id}
          installed={current.installed}
          command={current.command}
          model={model}
          effort={effort}
          onModel={onModel}
          onEffort={onEffort}
        />
      )}
      <div className="info-callout"><Icon name="brain" size={16} tiled /><span>Sign-in and tokens live in Settings. The terminal stays hidden during normal use.</span></div>
      <p className="trademark-note">{TRADEMARK_NOTICE}</p>
    </div>
  );
}

function PlannerModelFields({
  provider,
  installed,
  command,
  model,
  effort,
  onModel,
  onEffort,
}: {
  provider: string;
  installed: boolean;
  command: string;
  model: string;
  effort: string;
  onModel: (value: string) => void;
  onEffort: (value: string) => void;
}) {
  const [catalog, setCatalog] = useState<ProviderModelCatalog | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const requestId = useRef(0);

  const onEffortRef = useRef(onEffort);
  onEffortRef.current = onEffort;

  const load = useCallback(async () => {
    const ticket = ++requestId.current;
    if (!installed) {
      setCatalog(null);
      setError("");
      setLoading(false);
      return;
    }
    setLoading(true);
    setError("");
    try {
      const result = await invoke<ProviderModelCatalog>("list_provider_models", { provider });
      if (requestId.current !== ticket) return;
      setCatalog(result);
      setError(result.error ?? "");
    } catch (caught) {
      if (requestId.current !== ticket) return;
      setCatalog(null);
      setError(String(caught));
    } finally {
      if (requestId.current === ticket) setLoading(false);
    }
  }, [installed, provider]);

  useEffect(() => {
    setCatalog(null);
    setError("");
    void load();
  }, [load]);

  const effortMode = catalog?.effortMode ?? "";
  const effortNeedsModel = effortMode === "model-param";
  const modelOptions = catalog?.models.slice() ?? [];
  if (model && !modelOptions.some((item) => item.id === model)) {
    modelOptions.push({ id: model, displayName: `${model} (saved)`, efforts: [] });
  }
  const effortOptions = catalog ? effortsForModel(catalog, model).slice() : [];
  const showEffort = Boolean(
    catalog &&
      effortOptions.length > 0 &&
      (!effortNeedsModel || model),
  );

  useEffect(() => {
    if (!catalog) return;
    if (effortNeedsModel && !model) {
      if (effort) onEffortRef.current("");
      return;
    }
    const options = effortsForModel(catalog, model);
    if (effort && !options.some((item) => item.id === effort) && !catalog.error) {
      onEffortRef.current("");
    }
  }, [catalog, effort, effortNeedsModel, model]);

  return (
    <div className="planner-fields">
      <div className="planner-fields-head">
        <div>
          <strong>Model and thinking</strong>
          <span>
            {loading
              ? `Asking ${command} for its current list…`
              : installed
                ? `Fetched live from ${command}. Refresh after you update the CLI.`
                : `Install ${command} to choose a model.`}
          </span>
        </div>
        <button type="button" className="secondary-button" disabled={!installed || loading} onClick={() => void load()}>
          {loading ? "Refreshing…" : "Refresh"}
        </button>
      </div>
      <div className="planner-field-grid">
        <label>
          <span>Model</span>
          <select value={model} disabled={!installed || loading || modelOptions.length === 0} onChange={(event) => onModel(event.target.value)}>
            <option value="">{catalog?.defaultModel ? `CLI default (${catalog.defaultModel})` : "CLI default"}</option>
            {modelOptions.map((item) => (
              <option key={item.id} value={item.id}>{item.displayName || item.id}</option>
            ))}
          </select>
        </label>
        {showEffort && (
          <label>
            <span>Thinking effort</span>
            <select value={effortOptions.some((item) => item.id === effort) ? effort : ""} disabled={!installed || loading || effortOptions.length === 0} onChange={(event) => onEffort(event.target.value)}>
              <option value="">CLI default</option>
              {effortOptions.map((item) => (
                <option key={item.id} value={item.id}>{item.description ? `${item.id} — ${item.description}` : item.id}</option>
              ))}
            </select>
          </label>
        )}
      </div>
      {error && <p className="planner-fields-error">{error}</p>}
    </div>
  );
}

function LibraryStep({ path, onPath, onChoose, retention, onRetention }: { path: string; onPath: (path: string) => void; onChoose: () => void; retention: string; onRetention: (value: "all" | "failures" | "none") => void }) {
  return (
    <div className="setup-step">
      <span className="eyebrow">Workspace</span>
      <h1>Where should workflows live?</h1>
      <p className="setup-lead">Pick any folder you control. Screenshots stay on this machine.</p>
      <label className="path-picker">
        <span>Workflow folder</span>
        <div>
          <Icon name="folder" size={16} tiled />
          <input value={path} onChange={(event) => onPath(event.target.value)} />
          <button type="button" onClick={onChoose}>Browse</button>
        </div>
      </label>
      <div className="retention-group">
        <label>Run screenshots</label>
        {([
          ["failures", "Keep only when a run needs attention", "Best everyday default"],
          ["all", "Keep every run", "Useful if you need an audit trail"],
          ["none", "Discard after each action", "Most private"],
        ] as const).map(([value, label, hint]) => (
          <button className={retention === value ? "retention-option selected" : "retention-option"} onClick={() => onRetention(value)} key={value} type="button">
            <i>{retention === value && <span />}</i>
            <div>
              <strong>{label}</strong>
              <span>{hint}</span>
            </div>
          </button>
        ))}
      </div>
      <div className="ready-banner">
        <Icon name="shield" size={16} tiled />
        <div>
          <strong>Deletion stays blocked</strong>
          <span>Alfred Core refuses trash, purge, and overwrite. You can pause or take over from any run.</span>
        </div>
      </div>
    </div>
  );
}

function SettingsView({ settings, providers, system, onSave }: { settings: AppSettings; providers: ProviderStatus[]; system: SystemInfo; onSave: (settings: AppSettings) => void }) {
  const [draft, setDraft] = useState(() => withPlannerMaps(settings));
  const [saved, setSaved] = useState(false);
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [credentialSaved, setCredentialSaved] = useState<Record<string, boolean>>(() => Object.fromEntries(providers.map((item) => [item.id, item.credentialStored])));
  const [browserConnected, setBrowserConnected] = useState(false);
  const [message, setMessage] = useState("");
  useEffect(() => {
    invoke<boolean>("browser_bridge_status").then(setBrowserConnected).catch(() => setBrowserConnected(false));
  }, []);
  const choose = async () => {
    const selected = await open({ directory: true, multiple: false });
    if (typeof selected === "string") setDraft({ ...draft, libraryPath: selected });
  };
  const saveCredential = async (provider: ProviderStatus) => {
    try {
      await invoke("store_provider_secret", { provider: provider.id, secret: secrets[provider.id] ?? "" });
      setCredentialSaved((current) => ({ ...current, [provider.id]: true }));
      setSecrets((current) => ({ ...current, [provider.id]: "" }));
      setMessage(`${provider.name} credential saved in the OS vault.`);
    } catch (caught) {
      setMessage(String(caught));
    }
  };
  return (
    <div className="page settings-page">
      <section className="page-title">
        <div>
          <h1>Settings</h1>
          <p>The brain, this PC, and where files live. Goals always use the brain you pick here.</p>
        </div>
        <div className="page-title-actions">
          <span>{saved ? "Saved on this machine." : "Changes stay on this machine."}</span>
          <button className="primary-button" onClick={async () => { await onSave(draft); setSaved(true); setTimeout(() => setSaved(false), 1800); }}>Save changes</button>
        </div>
      </section>
      <div className="settings-layout">
        <section className="settings-section">
          <h2>Brain</h2>
          <p>Alfred asks this CLI for the next step. Pick once. Home no longer asks on every goal.</p>
          <div className="provider-settings">
            {providers.map((provider) => (
              <div className={draft.provider === provider.id ? "provider-setting selected" : "provider-setting"} key={provider.id}>
                <button type="button" onClick={() => setDraft({ ...draft, provider: provider.id })}>
                  <ProviderMark id={provider.id} size={16} small />
                  <div>
                    <strong>{provider.name}</strong>
                    <small>by {providerOwner(provider.id)} · {provider.installed ? `${provider.version ?? provider.command} found` : `${provider.command} not found`}</small>
                  </div>
                  <i>{draft.provider === provider.id && <Icon name="check" size={13} />}</i>
                </button>
                {draft.provider === provider.id && (
                  <>
                    <div className="credential-row">
                      <input
                        type="password"
                        autoComplete="off"
                        value={secrets[provider.id] ?? ""}
                        onChange={(event) => setSecrets((current) => ({ ...current, [provider.id]: event.target.value }))}
                        placeholder={credentialSaved[provider.id] ? "Saved in the vault · enter to replace" : "Optional API token"}
                      />
                      <button type="button" disabled={!secrets[provider.id]} onClick={() => saveCredential(provider)}>Save to vault</button>
                    </div>
                    <PlannerModelFields
                      provider={provider.id}
                      installed={provider.installed}
                      command={provider.command}
                      model={plannerChoice(draft, provider.id, "model")}
                      effort={plannerChoice(draft, provider.id, "effort")}
                      onModel={(value) => setDraft((current) => setPlannerChoice(current, provider.id, "model", value))}
                      onEffort={(value) => setDraft((current) => setPlannerChoice(current, provider.id, "effort", value))}
                    />
                  </>
                )}
              </div>
            ))}
          </div>
          <p className="trademark-note">{TRADEMARK_NOTICE}</p>
        </section>
        <section className="settings-section">
          <h2>This PC</h2>
          <p>Native host: <strong>{system.nativeHost}</strong>. Safe actions run; deletion stays blocked.</p>
          <div className="bridge-status">
            <i className={browserConnected ? "connected" : ""} />
            <div>
              <strong>{browserConnected ? "Browser helper connected" : "Windows host is driving the browser"}</strong>
              <span>{browserConnected ? "DOM-backed clicks are available on top of native control." : "The extension is optional. Alfred can still operate the browser from the desktop."}</span>
            </div>
            <button type="button" onClick={async () => setBrowserConnected(await invoke<boolean>("browser_bridge_status"))}>Check</button>
          </div>
        </section>
        <section className="settings-section">
          <h2>Workflow folder</h2>
          <p>Saved workflows are ordinary files in a folder you own.</p>
          <div className="settings-path">
            <Icon name="folder" size={16} tiled />
            <span>{draft.libraryPath}</span>
            <button type="button" onClick={choose}>Change</button>
          </div>
          <small className="platform-note">{system.os} · {system.architecture}</small>
        </section>
        <section className="settings-section">
          <h2>Screenshots</h2>
          <select value={draft.screenshotRetention} onChange={(event) => setDraft({ ...draft, screenshotRetention: event.target.value as AppSettings["screenshotRetention"] })}>
            <option value="failures">Keep only when a run needs attention</option>
            <option value="all">Keep every run</option>
            <option value="none">Discard immediately</option>
          </select>
          <label className="toggle-row">
            <input type="checkbox" checked={draft.shareScreenshotsWithPlanner} onChange={(event) => setDraft({ ...draft, shareScreenshotsWithPlanner: event.target.checked })} />
            <span>Share the target-app screenshot with the selected brain</span>
          </label>
        </section>
        <section className="settings-section">
          <div className="settings-section-head">
            <div>
              <h2>Local logs</h2>
              <p>A JSONL file for each planner turn. Nothing is uploaded. Turn this on when a run fails.</p>
            </div>
            <button className="secondary-button" onClick={async () => {
              try {
                const folder = await invoke<string>("run_logs_folder");
                setMessage(`Logs are in ${folder}`);
              } catch (caught) {
                setMessage(String(caught));
              }
            }}>Show log folder</button>
          </div>
          <label className="toggle-row">
            <input type="checkbox" checked={draft.diagnosticLogging} onChange={(event) => setDraft({ ...draft, diagnosticLogging: event.target.checked })} />
            <span>Record planner and tool-call logs on this machine</span>
          </label>
        </section>
        <section className="settings-section">
          <h2>Safety</h2>
          <div className="immutable-setting">
            <Icon name="shield" size={16} tiled />
            <div>
              <strong>Persistent-data deletion blocked</strong>
              <span>Enforced in Core, the Windows host, and the browser extension.</span>
            </div>
            <span>Always on</span>
          </div>
        </section>
        <section className="settings-section">
          <div className="settings-section-head">
            <div>
              <h2>Setup</h2>
              <p>Walk through Welcome, Brain, and Workspace again. Your current folder and brain stay as the starting draft.</p>
            </div>
            <button className="secondary-button" onClick={async () => { await onSave({ ...draft, onboardingComplete: false }); }}>Show setup again</button>
          </div>
        </section>
      </div>
      {message && <div className="info-callout"><Icon name="check" size={16} tiled /><span>{message}</span></div>}
    </div>
  );
}

export default App;
