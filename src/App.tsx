import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { Icon } from "./icons";
import type { AppSettings, PermissionGrant, ProviderEvent, ProviderStatus, RunCheckpoint, RunEvent, SystemInfo, View, Workflow, WorkflowSchedule, WorkflowStep } from "./types";

const starterWorkflows: Workflow[] = [
  {
    id: "starter-invoices",
    name: "Weekly invoice summary",
    goal: "Collect new supplier invoices, append them to a workbook, and prepare a summary.",
    version: "0.1.0",
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    status: "example",
    requiredApps: ["Microsoft Edge", "Microsoft Excel"],
    steps: [],
  },
  {
    id: "starter-research",
    name: "Website to workbook",
    goal: "Collect approved information from a website and append it to a local workbook.",
    version: "0.1.0",
    createdAt: new Date().toISOString(),
    updatedAt: new Date().toISOString(),
    status: "example",
    requiredApps: ["Microsoft Edge", "Microsoft Excel"],
    steps: [],
  },
];

function relativeDate(date: string) {
  const minutes = Math.max(1, Math.round((Date.now() - new Date(date).getTime()) / 60000));
  if (minutes < 60) return `${minutes}m ago`;
  if (minutes < 1440) return `${Math.round(minutes / 60)}h ago`;
  return `${Math.round(minutes / 1440)}d ago`;
}

function App() {
  const [loading, setLoading] = useState(true);
  const [system, setSystem] = useState<SystemInfo | null>(null);
  const [settings, setSettings] = useState<AppSettings | null>(null);
  const [providers, setProviders] = useState<ProviderStatus[]>([]);
  const [workflows, setWorkflows] = useState<Workflow[]>([]);
  const [view, setView] = useState<View>("home");
  const [createOpen, setCreateOpen] = useState(false);
  const [activeWorkflow, setActiveWorkflow] = useState<Workflow | null>(null);

  const refreshWorkflows = useCallback(async (libraryPath: string) => {
    const result = await invoke<Workflow[]>("list_workflows", { libraryPath });
    setWorkflows(result);
  }, []);

  useEffect(() => {
    Promise.all([
      invoke<SystemInfo>("get_system_info"),
      invoke<AppSettings>("get_settings"),
      invoke<ProviderStatus[]>("detect_providers"),
    ])
      .then(async ([systemResult, settingsResult, providerResult]) => {
        setSystem(systemResult);
        setSettings(settingsResult);
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
      <Sidebar view={view} onView={setView} onCreate={() => setCreateOpen(true)} />
      <main className="main-stage">
        <TopBar provider={settings.provider} providers={providers} onCreate={() => setCreateOpen(true)} />
        {activeWorkflow ? (
          <RunCockpit workflow={activeWorkflow} settings={settings} onWorkflowChanged={async () => refreshWorkflows(settings.libraryPath)} onClose={() => setActiveWorkflow(null)} />
        ) : (
          <>
            {view === "home" && <Home workflows={workflows} onCreate={() => setCreateOpen(true)} onRun={setActiveWorkflow} />}
            {view === "workflows" && <WorkflowLibrary workflows={workflows} onCreate={() => setCreateOpen(true)} onRun={setActiveWorkflow} />}
            {view === "runs" && <RunsView workflows={workflows} onRun={setActiveWorkflow} />}
            {view === "schedules" && <SchedulesView workflows={workflows} />}
            {view === "settings" && (
              <SettingsView
                settings={settings}
                providers={providers}
                system={system}
                onSave={async (next) => {
                  const saved = await invoke<AppSettings>("save_settings", { settings: next });
                  setSettings(saved);
                  await refreshWorkflows(saved.libraryPath);
                }}
              />
            )}
          </>
        )}
      </main>
      {createOpen && (
        <CreateWorkflowModal
          libraryPath={settings.libraryPath}
          onClose={() => setCreateOpen(false)}
          onCreated={(workflow) => {
            setWorkflows((current) => [workflow, ...current]);
            setCreateOpen(false);
            setActiveWorkflow(workflow);
          }}
        />
      )}
    </div>
  );
}

function LoadingScreen() {
  return (
    <div className="loading-screen">
      <div className="alfred-mark large"><Icon name="sparkle" size={28} /></div>
      <div className="loading-dots"><span /><span /><span /></div>
    </div>
  );
}

function Sidebar({ view, onView, onCreate }: { view: View; onView: (view: View) => void; onCreate: () => void }) {
  const items: { id: View; label: string; icon: string }[] = [
    { id: "home", label: "Home", icon: "home" },
    { id: "workflows", label: "Workflows", icon: "workflow" },
    { id: "runs", label: "Runs", icon: "runs" },
    { id: "schedules", label: "Schedules", icon: "workflow" },
  ];
  return (
    <aside className="sidebar">
      <div className="brand"><div className="alfred-mark"><Icon name="sparkle" size={18} /></div><span>Alfred</span></div>
      <button className="new-workflow-button" onClick={onCreate}><Icon name="plus" size={18} /> New workflow</button>
      <nav>
        {items.map((item) => (
          <button key={item.id} className={view === item.id ? "nav-item active" : "nav-item"} onClick={() => onView(item.id)}>
            <Icon name={item.icon} size={19} /><span>{item.label}</span>
          </button>
        ))}
      </nav>
      <div className="sidebar-spacer" />
      <div className="safety-card"><Icon name="shield" size={18} /><div><strong>Safety is active</strong><span>Deletion is always blocked</span></div></div>
      <button className={view === "settings" ? "nav-item active" : "nav-item"} onClick={() => onView("settings")}>
        <Icon name="settings" size={19} /><span>Settings</span>
      </button>
      <div className="profile-row"><div className="avatar">YG</div><div><strong>Your workspace</strong><span>Local and private</span></div></div>
    </aside>
  );
}

function TopBar({ provider, providers, onCreate }: { provider: string; providers: ProviderStatus[]; onCreate: () => void }) {
  const providerName = providers.find((item) => item.id === provider)?.name ?? provider;
  return (
    <header className="topbar">
      <div className="search-box"><Icon name="search" size={17} /><span>Search workflows and runs</span><kbd>⌘ K</kbd></div>
      <div className="topbar-actions"><span className="engine-status"><i />{providerName}</span><button className="icon-button" onClick={onCreate}><Icon name="plus" size={20} /></button></div>
    </header>
  );
}

function Home({ workflows, onCreate, onRun }: { workflows: Workflow[]; onCreate: () => void; onRun: (workflow: Workflow) => void }) {
  const visible = workflows.length ? workflows.slice(0, 4) : starterWorkflows;
  return (
    <div className="page home-page">
      <section className="hero-row">
        <div><span className="eyebrow">YOUR AUTOMATION WORKSPACE</span><h1>Good morning.</h1><p>What would you like Alfred to take care of?</p></div>
        <button className="primary-button" onClick={onCreate}><Icon name="plus" size={18} /> Create workflow</button>
      </section>
      <section className="command-card">
        <div className="command-icon"><Icon name="sparkle" size={22} /></div>
        <div className="command-copy"><strong>Describe your everyday task</strong><span>Alfred will plan it, show every action, and help you refine it.</span></div>
        <button onClick={onCreate}>Start describing <Icon name="arrow" size={17} /></button>
      </section>
      <GoalLauncher onRun={onRun} />
      <section className="section-block">
        <div className="section-heading"><div><h2>{workflows.length ? "Your workflows" : "See how Alfred works"}</h2><p>{workflows.length ? "Continue where you left off or run something again." : "Try a safe simulation, then create your own workflow."}</p></div><button className="text-button">View all <Icon name="arrow" size={15} /></button></div>
        <div className="workflow-grid">
          {visible.map((workflow, index) => <WorkflowCard key={workflow.id} workflow={workflow} index={index} onRun={() => onRun(workflow)} />)}
        </div>
      </section>
      <section className="insight-strip">
        <div className="insight-icon"><Icon name="shield" size={22} /></div>
        <div><strong>Every action passes through Alfred’s safety engine</strong><span>Persistent-data deletion is blocked before a provider or automation host can perform it.</span></div>
        <button>Review protection</button>
      </section>
    </div>
  );
}

function GoalLauncher({ onRun }: { onRun: (workflow: Workflow) => void }) {
  const [goal, setGoal] = useState("");
  const [apps, setApps] = useState("");
  const start = () => {
    const applications = apps.split(",").map((app) => app.trim()).filter(Boolean);
    if (!goal.trim()) return;
    const now = new Date().toISOString();
    onRun({
      id: `goal-${crypto.randomUUID()}`,
      name: goal.trim().length > 48 ? `${goal.trim().slice(0, 48)}…` : goal.trim(),
      goal: goal.trim(),
      version: "1.0.0",
      createdAt: now,
      updatedAt: now,
      status: "goal",
      requiredApps: applications,
      steps: [],
    });
  };
  return (
    <section className="command-card goal-launcher">
      <div className="command-icon"><Icon name="brain" size={22} /></div>
      <div className="command-copy goal-launcher-fields">
        <strong>Run a goal with the live planner</strong>
        <span>The planner observes your apps and acts step by step, with Alfred's safety engine and your approvals supervising every action.</span>
        <input value={goal} onChange={(event) => setGoal(event.target.value)} placeholder="Goal, e.g. Copy the invoice total from the open Edge page into Notepad" />
        <input value={apps} onChange={(event) => setApps(event.target.value)} placeholder="Target apps (optional) — leave blank and Alfred infers them from your goal" />
      </div>
      <button disabled={!goal.trim()} onClick={start}>Run goal <Icon name="arrow" size={17} /></button>
    </section>
  );
}

function WorkflowCard({ workflow, index, onRun }: { workflow: Workflow; index: number; onRun: () => void }) {
  const palettes = ["violet", "blue", "amber", "teal"];
  return (
    <article className="workflow-card">
      <div className={`workflow-art ${palettes[index % palettes.length]}`}><Icon name={index % 2 ? "monitor" : "workflow"} size={25} /></div>
      <div className="workflow-card-copy"><div className="workflow-title-row"><h3>{workflow.name}</h3><span className={`status-badge ${workflow.status}`}>{workflow.status}</span></div><p>{workflow.goal}</p></div>
      <div className="app-chips">{(workflow.requiredApps.length ? workflow.requiredApps : ["Apps chosen during planning"]).map((app) => <span key={app}>{app}</span>)}</div>
      <div className="workflow-card-footer"><span>{workflow.status === "example" ? "Safe simulation" : `Updated ${relativeDate(workflow.updatedAt)}`}</span><button onClick={onRun}><Icon name="runs" size={15} /> Run</button></div>
    </article>
  );
}

function WorkflowLibrary({ workflows, onCreate, onRun }: { workflows: Workflow[]; onCreate: () => void; onRun: (workflow: Workflow) => void }) {
  return (
    <div className="page">
      <section className="page-title"><div><span className="eyebrow">LOCAL LIBRARY</span><h1>Workflows</h1><p>Your automations stay in the folder you chose.</p></div><button className="primary-button" onClick={onCreate}><Icon name="plus" size={18} /> Create workflow</button></section>
      {workflows.length === 0 ? (
        <EmptyState icon="folder" title="Your workflow library is empty" description="Describe a repetitive task and Alfred will help you turn a successful run into a reusable workflow." action="Create your first workflow" onAction={onCreate} />
      ) : (
        <div className="workflow-list">{workflows.map((workflow, index) => <WorkflowCard key={workflow.id} workflow={workflow} index={index} onRun={() => onRun(workflow)} />)}</div>
      )}
    </div>
  );
}

function RunsView({ workflows, onRun }: { workflows: Workflow[]; onRun: (workflow: Workflow) => void }) {
  return (
    <div className="page">
      <section className="page-title"><div><span className="eyebrow">ACTIVITY</span><h1>Runs</h1><p>Watch current work and review the evidence from previous runs.</p></div></section>
      <EmptyState icon="runs" title="No runs yet" description="When Alfred runs a workflow, its action timeline, safety decisions, and results will appear here." action={workflows.length ? "Run a workflow" : "Create a workflow"} onAction={() => workflows[0] && onRun(workflows[0])} />
    </div>
  );
}

function SchedulesView({ workflows }: { workflows: Workflow[] }) {
  const [schedules, setSchedules] = useState<WorkflowSchedule[]>([]);
  const [workflowId, setWorkflowId] = useState(workflows[0]?.id ?? "");
  const [time, setTime] = useState("09:00");
  const [error, setError] = useState("");
  const refresh = useCallback(() => invoke<WorkflowSchedule[]>("list_schedules").then(setSchedules), []);
  useEffect(() => { refresh(); }, [refresh]);
  const add = async () => {
    const workflow = workflows.find(item => item.id === workflowId); if (!workflow) return;
    const [hour, minute] = time.split(":").map(Number);
    try { await invoke("save_schedule", { workflowId, workflowName: workflow.name, hour, minute, days: [0,1,2,3,4] }); await refresh(); }
    catch (caught) { setError(String(caught)); }
  };
  return <div className="page"><section className="page-title"><div><span className="eyebrow">LOCAL AUTOMATION</span><h1>Schedules</h1><p>On Windows, saved schedules are registered with Task Scheduler for unattended runs. Other platforms run them while Alfred is open.</p></div></section>
    <section className="settings-section schedule-builder"><h2>New weekday schedule</h2><div className="schedule-form"><select value={workflowId} onChange={event => setWorkflowId(event.target.value)}><option value="">Choose workflow</option>{workflows.filter(item => item.status !== "recording").map(item => <option value={item.id} key={item.id}>{item.name}</option>)}</select><input type="time" value={time} onChange={event => setTime(event.target.value)}/><button className="primary-button" disabled={!workflowId} onClick={add}>Add schedule</button></div>{error && <div className="error-message">{error}</div>}</section>
    <div className="schedule-list">{schedules.map(schedule => <article className="settings-section schedule-row" key={schedule.id}><div><strong>{schedule.workflowName}</strong><span>Weekdays at {String(schedule.hour).padStart(2,"0")}:{String(schedule.minute).padStart(2,"0")}</span></div><button className={schedule.enabled ? "status-badge ready" : "status-badge"} onClick={async () => { const next = await invoke<WorkflowSchedule[]>("set_schedule_enabled", { scheduleId: schedule.id, enabled: !schedule.enabled }); setSchedules(next); }}>{schedule.enabled ? "Enabled" : "Disabled"}</button></article>)}</div>
    {!schedules.length && <EmptyState icon="runs" title="No schedules yet" description="Finalize a workflow, then choose when it should run." action="Create after finalizing a workflow" onAction={() => {}}/>}
  </div>;
}

function EmptyState({ icon, title, description, action, onAction }: { icon: string; title: string; description: string; action: string; onAction: () => void }) {
  return <div className="empty-state"><div className="empty-icon"><Icon name={icon} size={29} /></div><h2>{title}</h2><p>{description}</p><button className="primary-button" onClick={onAction}>{action}</button></div>;
}

function CreateWorkflowModal({ libraryPath, onClose, onCreated }: { libraryPath: string; onClose: () => void; onCreated: (workflow: Workflow) => void }) {
  const [name, setName] = useState("");
  const [goal, setGoal] = useState("");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const submit = async () => {
    setSaving(true); setError("");
    try {
      const workflow = await invoke<Workflow>("create_workflow", { libraryPath, name, goal });
      onCreated(workflow);
    } catch (caught) {
      setError(String(caught)); setSaving(false);
    }
  };
  return (
    <div className="modal-backdrop" onMouseDown={(event) => event.target === event.currentTarget && onClose()}>
      <div className="modal-panel">
        <div className="modal-heading"><div className="modal-mark"><Icon name="sparkle" size={22} /></div><div><h2>Create a workflow</h2><p>Describe the outcome. Alfred will help discover the steps.</p></div><button className="icon-button" onClick={onClose}><Icon name="close" size={19} /></button></div>
        <label className="field-label">Workflow name<input autoFocus value={name} onChange={(event) => setName(event.target.value)} placeholder="For example, Weekly invoice summary" /></label>
        <label className="field-label">What should Alfred accomplish?<textarea value={goal} onChange={(event) => setGoal(event.target.value)} placeholder="Open the supplier portal, collect new invoices, append them to my workbook, and prepare a summary…" rows={5} /></label>
        <div className="safety-note"><Icon name="shield" size={18} /><span>Alfred will show its plan before interacting with your applications. Deletion remains blocked.</span></div>
        {error && <div className="error-message">{error}</div>}
        <div className="modal-actions"><button className="secondary-button" onClick={onClose}>Cancel</button><button className="primary-button" disabled={saving || !name.trim() || !goal.trim()} onClick={submit}>{saving ? "Creating…" : "Create and preview"}<Icon name="arrow" size={16} /></button></div>
      </div>
    </div>
  );
}

function RunCockpit({ workflow, settings, onWorkflowChanged, onClose }: { workflow: Workflow; settings: AppSettings; onWorkflowChanged: () => Promise<void>; onClose: () => void }) {
  if (workflow.status === "recording") return <WorkflowStudio workflow={workflow} settings={settings} onWorkflowChanged={onWorkflowChanged} onClose={onClose}/>;
  return <ExecutionCockpit workflow={workflow} settings={settings} onClose={onClose}/>;
}

function WorkflowStudio({ workflow, settings, onWorkflowChanged, onClose }: { workflow: Workflow; settings: AppSettings; onWorkflowChanged: () => Promise<void>; onClose: () => void }) {
  const initialApplication = /notepad/i.test(workflow.goal) ? "Notepad" : /calculator/i.test(workflow.goal) ? "Calculator" : /edge|browser|website/i.test(workflow.goal) ? "Microsoft Edge" : "";
  const [steps, setSteps] = useState(workflow.steps);
  const [application, setApplication] = useState(initialApplication);
  const [method, setMethod] = useState(initialApplication ? "launchApplication" : "observeWindow");
  const [target, setTarget] = useState("");
  const [parameters, setParameters] = useState(initialApplication ? JSON.stringify({ application: initialApplication }, null, 2) : "{}");
  const [waitFor, setWaitFor] = useState("");
  const [expect, setExpect] = useState("");
  const [saveAs, setSaveAs] = useState("");
  const [providerLines, setProviderLines] = useState<string[]>([]);
  const [providerSession, setProviderSession] = useState("");
  const [proposedSteps, setProposedSteps] = useState<WorkflowStep[]>([]);
  const [proposedParameters, setProposedParameters] = useState<Record<string, string>>({});
  const [recordingPlan, setRecordingPlan] = useState(false);
  const [error, setError] = useState("");
  const activeProvider = useRef("");
  const providerOutput = useRef<string[]>([]);
  useEffect(() => {
    let dispose: (() => void) | undefined;
    listen<ProviderEvent>("alfred://provider-event", ({ payload }) => {
      if (payload.sessionId !== activeProvider.current) return;
      providerOutput.current = [...providerOutput.current.slice(-199), payload.line];
      setProviderLines(current => [...current.slice(-80), payload.line]);
      if (payload.status === "completed") {
        setProviderSession("");
        invoke<WorkflowStep[]>("parse_provider_plan", { output: providerOutput.current })
          .then(next => {
            setProposedSteps(next);
            setProposedParameters(Object.fromEntries(next.map(step => [step.id, JSON.stringify(step.payload ?? {}, null, 2)])));
          })
          .catch(caught => setError(`The provider finished, but its plan could not be reviewed: ${String(caught)}`));
      } else if (payload.status === "failed") {
        setProviderSession("");
        setError("The selected brain could not complete the planning session. Review its output and try again.");
      }
    }).then(value => dispose = value);
    return () => dispose?.();
  }, []);
  const askProvider = async () => {
    setError(""); setProviderLines([]); setProposedSteps([]); providerOutput.current = [];
    const prompt = `Plan a safe Windows desktop workflow for this goal: ${workflow.goal}
Return ONLY one JSON object with this exact shape:
{"steps":[{"title":"Human-readable action","application":"Notepad","method":"launchApplication","targetLabel":"Notepad","params":{}}]}
Allowed methods: launchApplication, focusApplication, observeWindow, captureWindow, findElement, getValue, invokeElement, setValue, click, typeText, key, browser.observe, browser.navigate, browser.click, browser.type, browser.getText.
For launchApplication use one of: Notepad, Calculator, Paint, File Explorer, Microsoft Edge, Google Chrome, Brave. Use {"text":"..."} for typeText. Never propose deletion, trash, purge, destructive overwrite, password entry, shell commands, credentials, or arbitrary executables.`;
    const sessionId = crypto.randomUUID();
    activeProvider.current = sessionId; setProviderSession(sessionId);
    try { await invoke<string>("start_provider_run", { provider: settings.provider, prompt, workingDirectory: settings.libraryPath, sessionId }); }
    catch (caught) { activeProvider.current = ""; setProviderSession(""); setError(String(caught)); }
  };
  const recordStep = async (step: WorkflowStep) => {
    const saved = await invoke<Workflow>("record_action", { libraryPath: settings.libraryPath, workflowId: workflow.id, step });
    if (step.effect !== "observe") await invoke("grant_permission", { application: step.kind.startsWith("browser.") ? "Installed browser" : step.application, allowedEffects: [step.effect], allowedIntents: [step.kind.split(".").at(-1)] });
    setSteps(saved.steps);
    return saved;
  };
  const addStep = async () => {
    setError("");
    try {
      if (!application.trim()) throw new Error("Choose an application.");
      const payload = JSON.parse(parameters);
      const effect = method.endsWith("observe") || method === "observeWindow" || method === "captureWindow" || method === "findElement" || method === "getValue" || method === "browser.getText" ? "observe" : "modify_reversible";
      await recordStep({
        id: crypto.randomUUID(), title: target || method, kind: method, effect, application,
        intent: `${method} ${target}`.trim(), targetLabel: target || undefined, payload, timeoutMs: 30000, retries: 1,
        waitFor: waitFor.trim() ? { name: waitFor.trim() } : undefined,
        expect: expect.trim() ? { name: expect.trim() } : undefined,
        saveAs: saveAs.trim() ? saveAs.trim() : undefined,
      });
      setTarget(""); setParameters("{}"); setWaitFor(""); setExpect(""); setSaveAs("");
    } catch (caught) { setError(`Could not record action: ${String(caught)}`); }
  };
  const approvePlan = async () => {
    setError(""); setRecordingPlan(true);
    try {
      const approved = proposedSteps.map(step => ({ ...step, intent: `${step.kind} ${step.targetLabel ?? ""}`.trim(), payload: JSON.parse(proposedParameters[step.id] ?? "{}") }));
      const saved = await invoke<Workflow>("record_actions", { libraryPath: settings.libraryPath, workflowId: workflow.id, steps: approved });
      for (const step of approved.filter(step => step.effect !== "observe")) {
        await invoke("grant_permission", { application: step.kind.startsWith("browser.") ? "Installed browser" : step.application, allowedEffects: [step.effect], allowedIntents: [step.kind.split(".").at(-1)] });
      }
      setSteps(saved.steps);
      setProposedSteps([]);
    } catch (caught) { setError(`Could not record the approved plan: ${String(caught)}`); }
    finally { setRecordingPlan(false); }
  };
  const finalize = async () => { try { await invoke("finalize_recording", { libraryPath: settings.libraryPath, workflowId: workflow.id }); await onWorkflowChanged(); onClose(); } catch (caught) { setError(String(caught)); } };
  return <div className="page workflow-studio"><section className="page-title"><div><button className="back-button" onClick={onClose}>‹</button><span className="eyebrow">WORKFLOW RECORDER</span><h1>{workflow.name}</h1><p>{workflow.goal}</p></div><button className="primary-button" disabled={!steps.length} onClick={finalize}>Finalize workflow</button></section>
    <div className="studio-grid"><section className="settings-section"><h2>1. Ask the selected brain</h2><p>Alfred runs the installed {settings.provider} CLI in a supervised, read-only planning session.</p><div className="studio-actions"><button className="secondary-button" disabled={!!providerSession} onClick={askProvider}><Icon name="brain" size={17}/> {providerSession ? "Planning…" : "Generate plan"}</button>{providerSession && <button className="secondary-button" onClick={async () => { await invoke("cancel_provider_run", { sessionId: providerSession }); setProviderSession(""); }}>Stop planner</button>}</div><pre className="provider-console">{providerLines.length ? providerLines.join("\n") : "Provider output appears here. Alfred will extract safe actions for your review."}</pre></section>
      <section className="settings-section"><h2>2. Add an action manually</h2><p>Use this only to refine the provider plan. Parameters stay in the portable workflow YAML.</p><div className="record-form"><label>Application<input value={application} onChange={event => setApplication(event.target.value)} placeholder="For example, Notepad"/></label><label>Method<select value={method} onChange={event => setMethod(event.target.value)}><option>launchApplication</option><option>focusApplication</option><option>observeWindow</option><option>captureWindow</option><option>findElement</option><option>getValue</option><option>invokeElement</option><option>setValue</option><option>click</option><option>typeText</option><option>key</option><option>browser.observe</option><option>browser.navigate</option><option>browser.click</option><option>browser.type</option><option>browser.getText</option></select></label><label>Target label<input value={target} onChange={event => setTarget(event.target.value)} placeholder="For example, Notepad editor"/></label><label>Parameters (JSON)<textarea value={parameters} onChange={event => setParameters(event.target.value)} rows={4}/></label><label>Wait for (optional label)<input value={waitFor} onChange={event => setWaitFor(event.target.value)} placeholder="Results table"/></label><label>Expect after (optional label)<input value={expect} onChange={event => setExpect(event.target.value)} placeholder="Saved confirmation"/></label><label>Save result as (optional variable)<input value={saveAs} onChange={event => setSaveAs(event.target.value)} placeholder="invoiceNumber"/></label><button className="primary-button" onClick={addStep}>Record action</button></div></section></div>
    {error && <div className="error-message">{error}</div>}
    {proposedSteps.length > 0 && <section className="panel-surface proposed-plan"><div className="panel-heading"><span>Review the proposed plan</span><button className="primary-button" disabled={recordingPlan} onClick={approvePlan}>{recordingPlan ? "Recording…" : "Approve and record all"}</button></div><p>Every step has passed Alfred’s base safety policy. Edit any field before approving.</p>{proposedSteps.map((step, index) => <div className="proposed-step" key={step.id}><span className="step-marker">{index + 1}</span><div><input aria-label={`Step ${index + 1} title`} value={step.title} onChange={event => setProposedSteps(current => current.map(item => item.id === step.id ? { ...item, title: event.target.value } : item))}/><div className="proposed-step-fields"><input aria-label={`Step ${index + 1} application`} value={step.application ?? ""} onChange={event => setProposedSteps(current => current.map(item => item.id === step.id ? { ...item, application: event.target.value } : item))}/><select aria-label={`Step ${index + 1} method`} value={step.kind} onChange={event => setProposedSteps(current => current.map(item => item.id === step.id ? { ...item, kind: event.target.value, effect: event.target.value.endsWith("observe") || ["observeWindow","captureWindow"].includes(event.target.value) ? "observe" : "modify_reversible" } : item))}><option>launchApplication</option><option>focusApplication</option><option>observeWindow</option><option>captureWindow</option><option>invokeElement</option><option>click</option><option>typeText</option><option>key</option><option>browser.observe</option><option>browser.navigate</option><option>browser.click</option><option>browser.type</option></select></div><textarea aria-label={`Step ${index + 1} parameters`} value={proposedParameters[step.id] ?? "{}"} onChange={event => setProposedParameters(current => ({ ...current, [step.id]: event.target.value }))}/></div></div>)}</section>}
    <section className="panel-surface recorded-steps"><div className="panel-heading"><span>Recorded steps</span><b>{steps.length}</b></div>{steps.map((step, index) => <div className="plan-step done" key={step.id}><span className="step-marker">{index + 1}</span><div><strong>{step.title}</strong><small>{step.application} · {step.kind} · {step.effect}</small></div></div>)}{!steps.length && <p>No actions recorded yet.</p>}</section>
  </div>;
}

function ExecutionCockpit({ workflow, settings, onClose }: { workflow: Workflow; settings: AppSettings; onClose: () => void }) {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [runId, setRunId] = useState("");
  const [paused, setPaused] = useState(false);
  const [takeover, setTakeover] = useState(false);
  const [startError, setStartError] = useState("");
  const queued = useRef<RunEvent[]>([]);
  const activeRun = useRef("");
  // The backend starts emitting run events as soon as the run spawns, which can
  // beat the invoke() response that carries the run id. Hold those early events
  // instead of dropping them — a run that fails before the id arrives must not
  // leave the cockpit spinning forever. Only one run drives the machine at a
  // time (the core's run lock), so early events can only belong to this run.
  const early = useRef<RunEvent[]>([]);
  const pausedRef = useRef(false);
  const takeoverRef = useRef(false);

  useEffect(() => {
    pausedRef.current = paused;
    takeoverRef.current = takeover;
  }, [paused, takeover]);

  // Subscribe once: re-subscribing on pause/takeover would open a gap in which
  // events are silently missed.
  useEffect(() => {
    let dispose: (() => void) | undefined;
    let active = true;
    listen<RunEvent>("alfred://run-event", ({ payload }) => {
      if (!active) return;
      if (payload.runId !== activeRun.current) {
        if (!activeRun.current) early.current.push(payload);
        return;
      }
      if (pausedRef.current || takeoverRef.current) queued.current.push(payload);
      else setEvents((current) => [...current, payload]);
    }).then((unlisten) => { if (active) dispose = unlisten; else unlisten(); });
    return () => { active = false; dispose?.(); };
  }, []);

  useEffect(() => {
    const command = workflow.status === "example" ? "start_demo_run" : workflow.status === "goal" ? "start_goal_run" : "start_workflow_run";
    const args = workflow.status === "example"
      ? { workflowId: workflow.id }
      : workflow.status === "goal"
        ? { goal: workflow.goal, applications: workflow.requiredApps }
        : { libraryPath: settings.libraryPath, workflowId: workflow.id, resumeRunId: null };
    invoke<string>(command, args).then((id) => {
      activeRun.current = id;
      setRunId(id);
      const held = early.current.filter((event) => event.runId === id);
      early.current = [];
      if (held.length) setEvents((current) => [...current, ...held]);
    }).catch((caught) => setStartError(String(caught)));
    return () => { activeRun.current = ""; early.current = []; };
  }, [workflow.id, workflow.status, workflow.goal, workflow.requiredApps, settings.libraryPath]);

  // Silence watchdog: if the run reached a terminal state before any timeline
  // event could be displayed, surface its checkpoint instead of spinning with
  // no explanation.
  useEffect(() => {
    if (!runId || events.length > 0 || startError || workflow.status === "example") return;
    let cancelled = false;
    const timer = window.setInterval(() => {
      invoke<RunCheckpoint | null>("get_checkpoint", { runId }).then((checkpoint) => {
        if (cancelled || !checkpoint || checkpoint.status === "running") return;
        if (checkpoint.status === "completed") {
          setEvents((current) => current.length ? current : [{
            runId,
            sequence: checkpoint.nextStepIndex,
            stepId: "recovered",
            title: "Run completed",
            detail: "The run finished before its timeline could be displayed.",
            application: "Alfred",
            status: "completed",
            progress: 100,
            timestamp: checkpoint.updatedAt,
          }]);
        } else {
          setStartError(`The run ended before it could show progress: ${checkpoint.error ?? "it was stopped."}`);
        }
      }).catch(() => undefined);
    }, 3000);
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [runId, events.length, startError, workflow.status]);

  const resume = () => {
    setPaused(false); setTakeover(false);
    if (runId && workflow.status !== "example") invoke("set_run_control", { runId, control: "running" });
    if (queued.current.length) { setEvents((current) => [...current, ...queued.current]); queued.current = []; }
  };
  const controlRun = (control: "paused" | "stop") => {
    if (runId && workflow.status !== "example") invoke("set_run_control", { runId, control }).catch(() => undefined);
    if (control === "paused") setPaused(true); else onClose();
  };
  const retryFromCheckpoint = async () => {
    try {
      setStartError("");
      const id = await invoke<string>("start_workflow_run", { libraryPath: settings.libraryPath, workflowId: workflow.id, resumeRunId: runId });
      activeRun.current = id; setPaused(false); setTakeover(false);
    } catch (caught) { setStartError(String(caught)); }
  };
  const approveWaitingStep = async () => {
    try { await invoke("approve_run_step", { runId }); }
    catch (caught) { setStartError(String(caught)); }
  };
  const current = events.at(-1);
  const progress = current?.progress ?? 3;
  const complete = progress === 100 && current?.status !== "failed";
  const waitingApproval = current?.status === "waiting";
  const planned = workflow.status === "goal"
    ? (events.length ? events.filter((event) => event.status === "completed").map((event) => event.title).slice(-8) : ["The planner decides each step live — actions appear here as they happen."])
    : workflow.steps.length ? workflow.steps.map(step => step.title) : ["Prepare workspace", "Open approved website", "Read invoice table", "Check safety policy", "Append workbook rows", "Verify the result"];

  return (
    <div className="cockpit">
      <div className="cockpit-header">
        <div><button className="back-button" onClick={onClose}>‹</button><span className="run-kicker">{complete ? "RUN COMPLETED" : waitingApproval ? "WAITING FOR APPROVAL" : takeover ? "USER IN CONTROL" : paused ? "RUN PAUSED" : "RUNNING SAFELY"}</span><h1>{workflow.name}</h1></div>
        <div className="run-controls">
          {current?.status === "failed" && workflow.status !== "example" && workflow.status !== "goal" && <button className="primary-button" onClick={retryFromCheckpoint}>Retry from checkpoint</button>}
          {(paused || takeover || current?.status === "paused") ? <button className="secondary-button" onClick={resume}><Icon name="runs" size={16} /> Resume</button> : <button className="secondary-button" onClick={() => controlRun("paused")}><Icon name="pause" size={16} /> Pause</button>}
          <button className="secondary-button" onClick={() => { controlRun("paused"); setTakeover(true); }}><Icon name="hand" size={16} /> Take over</button>
          <button className="danger-button" onClick={() => controlRun("stop")}><Icon name="stop" size={15} /> Stop</button>
        </div>
      </div>
      {startError && <div className="error-message">{startError}</div>}
      {waitingApproval && <div className="approval-banner panel-surface"><div><strong>Alfred needs your approval</strong><span>{current?.detail}</span></div><div className="run-controls"><button className="primary-button" onClick={approveWaitingStep}>Approve this action</button><button className="danger-button" onClick={() => controlRun("stop")}>Deny and stop</button></div></div>}
      <div className="progress-track"><span style={{ width: `${progress}%` }} /></div>
      <div className="cockpit-grid">
        <section className="plan-panel panel-surface">
          <div className="panel-heading"><span>Workflow</span><b>{progress}%</b></div>
          <div className="plan-list">
            {planned.map((step, index) => {
              const event = workflow.steps.length ? [...events].reverse().find(item => item.stepId === workflow.steps[index]?.id) : events.find(item => item.sequence === index);
              const done = event?.status === "completed"; const active = event?.status === "running" || event?.status === "failed" || (!event && index === 0);
              return <div key={`${step}-${index}`} className={`plan-step ${done ? "done" : active ? "current" : ""}`}><span className="step-marker">{done ? <Icon name="check" size={13} /> : index + 1}</span><div><strong>{step}</strong><small>{event ? `${event.application} · ${event.status}` : active ? "In progress" : "Waiting"}</small></div></div>;
            })}
          </div>
          <div className="policy-lock"><Icon name="lock" size={17} /><div><strong>Protected execution</strong><span>Deletion and irreversible actions are blocked.</span></div></div>
        </section>
        <section className="live-panel panel-surface">
          <div className="panel-heading"><span>Live application</span><div><span className="simulation-badge">{workflow.status === "example" ? "SIMULATION" : "NATIVE HOST"}</span><i className="live-dot" /> Live</div></div>
          <div className="screen-preview">
            {workflow.status === "example" ? <><div className="fake-browser-bar"><span className="browser-dots"><i /><i /><i /></span><div className="fake-address">supplier.example.com/invoices</div><span>⋯</span></div>
            <div className="fake-browser-content">
              <div className="fake-app-nav"><div className="fake-logo">S</div><span>Dashboard</span><span className="selected">Invoices</span><span>Reports</span></div>
              <div className="fake-page"><div className="fake-page-title"><div><small>FINANCE</small><strong>Supplier invoices</strong></div><button>Export</button></div>
                <div className={`fake-table ${progress >= 45 && progress < 70 ? "highlighted" : ""}`}>
                  <div className="fake-row header"><span>Invoice</span><span>Supplier</span><span>Date</span><span>Amount</span></div>
                  {["INV-1048|Northwind|Aug 7|₹24,800", "INV-1049|Contoso|Aug 7|₹18,250", "INV-1050|Fabrikam|Aug 8|₹31,400", "INV-1051|Adventure|Aug 8|₹12,990"].map((row) => <div className="fake-row" key={row}>{row.split("|").map((cell) => <span key={cell}>{cell}</span>)}</div>)}
                </div>
                {progress >= 45 && progress < 70 && <div className="focus-label">Alfred is reading 14 rows</div>}
              </div>
            </div></> : current?.evidenceDataUrl ? <img className="native-screenshot" src={current.evidenceDataUrl} alt={`Captured ${current.application} window`}/> : <div className="native-preview"><Icon name="monitor" size={38}/><strong>{current?.application ?? "Waiting for native host"}</strong><span>Screen evidence appears here after a capture step. Alfred continues to show each semantic action in Activity.</span></div>}
            {(paused || takeover) && <div className="paused-overlay"><div><Icon name={takeover ? "hand" : "pause"} size={28} /><strong>{takeover ? "You have control" : "Workflow paused"}</strong><span>Alfred will not interact until you resume.</span></div></div>}
          </div>
          <div className="now-doing"><div className="pulse-icon"><Icon name={complete ? "check" : "sparkle"} size={18} /></div><div><small>{complete ? "COMPLETED" : "ALFRED IS WORKING"}</small><strong>{current?.title ?? "Preparing the run"}</strong><span>{current?.detail ?? "Loading workflow context and checking permissions."}</span></div></div>
        </section>
        <section className="activity-panel panel-surface">
          <div className="panel-heading"><span>Activity</span><button>Evidence</button></div>
          <div className="event-list">
            {events.length === 0 && <div className="event-placeholder"><div className="mini-spinner" />Waiting for the first action…</div>}
            {[...events].reverse().map((event, index) => <div className="event-item" key={`${event.stepId}-${event.sequence}`}><span className={index === 0 && !complete ? "event-status active" : "event-status"}><Icon name={event.status === "completed" ? "check" : "sparkle"} size={13} /></span><div><strong>{event.title}</strong><p>{event.detail}</p><small>{new Date(event.timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" })} · {event.application}</small></div></div>)}
          </div>
          <div className="run-id">Run {runId ? runId.slice(0, 8) : "starting"} · Local only</div>
        </section>
      </div>
      <div className="steer-bar"><Icon name="sparkle" size={18} /><input placeholder="Tell Alfred something while it works…" /><span>Enter to send</span></div>
    </div>
  );
}

function Onboarding({ initialSettings, system, providers, onComplete }: { initialSettings: AppSettings; system: SystemInfo; providers: ProviderStatus[]; onComplete: (settings: AppSettings) => void }) {
  const [step, setStep] = useState(0);
  const [draft, setDraft] = useState<AppSettings>({ ...initialSettings, libraryPath: initialSettings.libraryPath || system.defaultLibraryPath });
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const steps = ["Welcome", "AI engine", "Computer access", "Workflow library", "Protection"];
  const chooseFolder = async () => {
    const selected = await open({ directory: true, multiple: false, title: "Choose your Alfred workflow library" });
    if (typeof selected === "string") setDraft((current) => ({ ...current, libraryPath: selected }));
  };
  const finish = async () => {
    setSaving(true); setError("");
    try {
      const saved = await invoke<AppSettings>("save_settings", { settings: { ...draft, onboardingComplete: true } });
      onComplete(saved);
    } catch (caught) { setError(String(caught)); setSaving(false); }
  };
  return (
    <div className="onboarding-shell">
      <aside className="onboarding-sidebar">
        <div className="brand light"><div className="alfred-mark"><Icon name="sparkle" size={18} /></div><span>Alfred</span></div>
        <div className="onboarding-progress">
          {steps.map((title, index) => <div className={`onboarding-step ${index === step ? "active" : index < step ? "done" : ""}`} key={title}><span>{index < step ? <Icon name="check" size={14} /> : index + 1}</span><div><strong>{title}</strong><small>{index === step ? "Current step" : index < step ? "Complete" : "Up next"}</small></div></div>)}
        </div>
        <div className="onboarding-assurance"><Icon name="shield" size={20} /><div><strong>Designed around control</strong><span>Alfred shows its work and blocks persistent-data deletion.</span></div></div>
      </aside>
      <main className="onboarding-main">
        <div className="onboarding-content">
          {step === 0 && <WelcomeStep system={system} />}
          {step === 1 && <ProviderStep providers={providers} selected={draft.provider} onSelect={(provider) => setDraft({ ...draft, provider })} />}
          {step === 2 && <AccessStep os={system.os} />}
          {step === 3 && <LibraryStep path={draft.libraryPath} onPath={(libraryPath) => setDraft({ ...draft, libraryPath })} onChoose={chooseFolder} retention={draft.screenshotRetention} onRetention={(screenshotRetention) => setDraft({ ...draft, screenshotRetention })} />}
          {step === 4 && <ProtectionStep />}
          {error && <div className="error-message">{error}</div>}
        </div>
        <div className="onboarding-footer"><button className="secondary-button" disabled={step === 0} onClick={() => setStep((current) => current - 1)}>Back</button><span>{step + 1} of {steps.length}</span>{step < steps.length - 1 ? <button className="primary-button" onClick={() => setStep((current) => current + 1)}>Continue <Icon name="arrow" size={16} /></button> : <button className="primary-button" disabled={saving} onClick={finish}>{saving ? "Saving…" : "Open Alfred"}<Icon name="arrow" size={16} /></button>}</div>
      </main>
    </div>
  );
}

function WelcomeStep({ system }: { system: SystemInfo }) {
  return <div className="setup-step welcome-step"><div className="welcome-visual"><div className="orbit one"/><div className="orbit two"/><div className="welcome-logo"><Icon name="sparkle" size={34}/></div><span className="floating-app edge">E</span><span className="floating-app excel">X</span><span className="floating-app outlook">O</span></div><span className="eyebrow">WELCOME TO ALFRED</span><h1>Everyday work,<br/>handled with care.</h1><p>Describe repetitive work across your applications. Alfred performs it visibly, helps you refine it, and saves the result as a reusable local workflow.</p><div className="platform-chip"><Icon name="monitor" size={17}/>{system.os === "windows" ? "Windows" : "macOS"} · {system.architecture}</div></div>;
}

function ProviderStep({ providers, selected, onSelect }: { providers: ProviderStatus[]; selected: string; onSelect: (id: string) => void }) {
  return <div className="setup-step"><span className="eyebrow">AI ENGINE</span><h1>Choose Alfred’s brain</h1><p className="setup-lead">Alfred supervises the selected engine and keeps computer access behind its safety boundary. You can change this later.</p><div className="provider-list">{providers.map((provider, index) => <button key={provider.id} className={selected === provider.id ? "provider-option selected" : "provider-option"} onClick={() => onSelect(provider.id)}><div className={`provider-logo provider-${provider.id}`}>{provider.name.charAt(provider.name === "OpenAI Codex" ? 7 : 0)}</div><div><strong>{provider.name}</strong><span>{provider.installed ? `Detected as ${provider.command}` : "Not detected · setup available later"}</span></div>{index === 0 && <em>Recommended</em>}<i>{selected === provider.id && <Icon name="check" size={14}/>}</i></button>)}</div><div className="info-callout"><Icon name="brain" size={19}/><span>The terminal stays hidden during normal use. Authentication and provider settings are handled through Alfred’s GUI.</span></div></div>;
}

function AccessStep({ os }: { os: string }) {
  const capabilities = os === "windows" ? [
    ["Screen capture", "See only the application Alfred is operating"],
    ["UI Automation", "Read buttons, fields, tables, and application state"],
    ["Keyboard and pointer", "Interact only after the safety engine approves"],
    ["Emergency stop", "Immediately release control from any application"],
  ] : [
    ["Screen Recording", "See only the application Alfred is operating"],
    ["Accessibility", "Read and operate approved application controls"],
    ["Keyboard and pointer", "Interact only after the safety engine approves"],
    ["Emergency stop", "Immediately release control from any application"],
  ];
  return <div className="setup-step"><span className="eyebrow">COMPUTER ACCESS</span><h1>Visible, scoped control</h1><p className="setup-lead">The native automation host will verify these capabilities. Alfred asks before using a new application.</p><div className="capability-list">{capabilities.map(([title, detail], index) => <div className="capability-row" key={title}><span className="capability-icon"><Icon name={index === 0 ? "monitor" : index === 3 ? "hand" : "check"} size={20}/></span><div><strong>{title}</strong><span>{detail}</span></div><span className="planned-badge">Verified at first use</span></div>)}</div><div className="shortcut-preview"><kbd>Ctrl</kbd><span>+</span><kbd>Shift</kbd><span>+</span><kbd>Esc</kbd><div><strong>Emergency stop</strong><span>This shortcut is reserved by Alfred.</span></div></div></div>;
}

function LibraryStep({ path, onPath, onChoose, retention, onRetention }: { path: string; onPath: (path: string) => void; onChoose: () => void; retention: string; onRetention: (value: "all"|"failures"|"none") => void }) {
  return <div className="setup-step"><span className="eyebrow">LOCAL STORAGE</span><h1>Choose your workflow library</h1><p className="setup-lead">Use Documents, OneDrive, Dropbox, a network drive, or any folder you control. Alfred does not require a workflow cloud.</p><label className="path-picker"><span>Workflow library</span><div><Icon name="folder" size={18}/><input value={path} onChange={(event) => onPath(event.target.value)}/><button onClick={onChoose}>Browse</button></div></label><div className="retention-group"><label>Run screenshots</label>{([["failures", "Keep only when a run needs attention"], ["all", "Keep evidence for every run"], ["none", "Discard after each action"]] as const).map(([value, label]) => <button className={retention === value ? "retention-option selected" : "retention-option"} onClick={() => onRetention(value)} key={value}><i>{retention === value && <span/>}</i><div><strong>{label}</strong><span>{value === "failures" ? "Recommended balance of privacy and diagnostics" : value === "all" ? "Useful for regulated or auditable workflows" : "Most private; less information for troubleshooting"}</span></div></button>)}</div></div>;
}

function ProtectionStep() {
  return <div className="setup-step"><span className="eyebrow">PROTECTION</span><h1>A boundary the AI cannot bypass</h1><p className="setup-lead">Every computer action is checked by Alfred Core before it reaches Windows, macOS, or your browser.</p><div className="protection-hero"><div className="shield-visual"><Icon name="shield" size={40}/></div><div><strong>Persistent-data protection</strong><span>Deletion, trash, purge, destructive overwrite, and disguised data-loss actions are hard-blocked.</span></div></div><div className="protection-grid"><div><Icon name="lock" size={20}/><strong>Least privilege</strong><span>Providers receive only the tools and context required for the current step.</span></div><div><Icon name="monitor" size={20}/><strong>Visible execution</strong><span>See the active application, current intent, and action evidence.</span></div><div><Icon name="hand" size={20}/><strong>Take over anytime</strong><span>Pause the workflow without losing completed progress.</span></div><div><Icon name="folder" size={20}/><strong>Local ownership</strong><span>Workflows remain in your chosen filesystem.</span></div></div><div className="ready-banner"><Icon name="check" size={20}/><div><strong>Alfred is ready to start</strong><span>You can revisit every setting from the application.</span></div></div></div>;
}

function SettingsView({ settings, providers, system, onSave }: { settings: AppSettings; providers: ProviderStatus[]; system: SystemInfo; onSave: (settings: AppSettings) => void }) {
  const [draft, setDraft] = useState(settings);
  const [saved, setSaved] = useState(false);
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [credentialSaved, setCredentialSaved] = useState<Record<string, boolean>>(() => Object.fromEntries(providers.map(item => [item.id, item.credentialStored])));
  const [permissions, setPermissions] = useState<PermissionGrant[]>([]);
  const [browserConnected, setBrowserConnected] = useState(false);
  const [permissionApp, setPermissionApp] = useState("Microsoft Excel");
  const [message, setMessage] = useState("");
  useEffect(() => {
    invoke<PermissionGrant[]>("list_permissions").then(setPermissions);
    invoke<boolean>("browser_bridge_status").then(setBrowserConnected).catch(() => setBrowserConnected(false));
  }, []);
  const choose = async () => { const selected = await open({ directory: true, multiple: false }); if (typeof selected === "string") setDraft({ ...draft, libraryPath: selected }); };
  const saveCredential = async (provider: ProviderStatus) => {
    try { await invoke("store_provider_secret", { provider: provider.id, secret: secrets[provider.id] ?? "" }); setCredentialSaved(current => ({ ...current, [provider.id]: true })); setSecrets(current => ({ ...current, [provider.id]: "" })); setMessage(`${provider.name} credential saved in the OS vault.`); }
    catch (caught) { setMessage(String(caught)); }
  };
  const addPermission = async () => {
    const next = await invoke<PermissionGrant>("grant_permission", { application: permissionApp, allowedEffects: ["modify_reversible"], allowedIntents: [] }); setPermissions(current => [...current, next]);
  };
  return <div className="page settings-page"><section className="page-title"><div><span className="eyebrow">PREFERENCES</span><h1>Settings</h1><p>Configure engines, credentials, computer access, and local storage without editing files.</p></div></section><div className="settings-layout">
    <section className="settings-section"><h2>AI engines and credentials</h2><p>CLI sessions use the provider's existing sign-in or a secret stored in Windows Credential Manager / macOS Keychain.</p><div className="provider-settings">{providers.map(provider => <div className={draft.provider === provider.id ? "provider-setting selected" : "provider-setting"} key={provider.id}><button onClick={() => setDraft({ ...draft, provider: provider.id })}><span className={`provider-logo small provider-${provider.id}`}>{provider.name.charAt(0)}</span><div><strong>{provider.name}</strong><small>{provider.installed ? `${provider.version ?? provider.command} detected` : `${provider.command} not found`}</small></div><i>{draft.provider === provider.id && <Icon name="check" size={13}/>}</i></button><div className="credential-row"><input type="password" autoComplete="off" value={secrets[provider.id] ?? ""} onChange={event => setSecrets(current => ({ ...current, [provider.id]: event.target.value }))} placeholder={credentialSaved[provider.id] ? "Credential stored · enter to replace" : "Optional API token"}/><button disabled={!secrets[provider.id]} onClick={() => saveCredential(provider)}>Save to vault</button></div></div>)}</div></section>
    <section className="settings-section"><h2>Computer and browser bridge</h2><p>Native host: <strong>{system.nativeHost}</strong></p><div className="bridge-status"><i className={browserConnected ? "connected" : ""}/><div><strong>{browserConnected ? "Installed browser connected" : "Browser bridge not connected"}</strong><span>{browserConnected ? "Semantic DOM actions and visible-tab capture are available." : "Load the Chromium extension and native host manifest, then reopen the browser."}</span></div><button onClick={async () => setBrowserConnected(await invoke<boolean>("browser_bridge_status"))}>Check</button></div></section>
    <section className="settings-section"><h2>Application permissions</h2><p>Alfred observes app state by default. Reversible changes require an explicit application grant.</p><div className="permission-add"><input value={permissionApp} onChange={event => setPermissionApp(event.target.value)} placeholder="Application name"/><button onClick={addPermission}>Allow reversible changes</button></div><div className="permission-list">{permissions.map(permission => <div key={permission.id}><div><strong>{permission.application}</strong><span>{permission.allowedEffects.join(", ")}</span></div><button onClick={async () => setPermissions(await invoke<PermissionGrant[]>("set_permission_enabled", { permissionId: permission.id, enabled: !permission.enabled }))}>{permission.enabled ? "Enabled" : "Disabled"}</button></div>)}</div></section>
    <section className="settings-section"><h2>Workflow library</h2><p>Automations are portable YAML files in a folder you control.</p><div className="settings-path"><Icon name="folder" size={18}/><span>{draft.libraryPath}</span><button onClick={choose}>Change</button></div><small className="platform-note">Running on {system.os} · {system.architecture}</small></section>
    <section className="settings-section"><h2>Screenshot retention</h2><select value={draft.screenshotRetention} onChange={(event) => setDraft({ ...draft, screenshotRetention: event.target.value as AppSettings["screenshotRetention"] })}><option value="failures">Only when a run needs attention</option><option value="all">Every run</option><option value="none">Discard immediately</option></select></section>
    <section className="settings-section"><h2>Planner vision</h2><p>Attach a screenshot of each target app to planner turns so the planner can see the desktop, not just read it. Images leave this device to the provider's API.</p><label className="toggle-row"><input type="checkbox" checked={draft.shareScreenshotsWithPlanner} onChange={(event) => setDraft({ ...draft, shareScreenshotsWithPlanner: event.target.checked })}/><span>Share screenshots with the planner (Codex and Copilot attach them directly; Grok and Cursor read the screenshot files Alfred lists in the prompt)</span></label></section>
    <section className="settings-section danger-zone"><h2>Safety policy</h2><div className="immutable-setting"><Icon name="shield" size={19}/><div><strong>Persistent-data deletion blocked</strong><span>This protection is enforced in Core, the Windows host, and the browser extension.</span></div><span>Always on</span></div></section>
  </div>{message && <div className="info-callout"><Icon name="check" size={18}/><span>{message}</span></div>}<div className="settings-save"><span>{saved ? "Settings saved" : "Changes stay local to this machine."}</span><button className="primary-button" onClick={async () => { await onSave(draft); setSaved(true); setTimeout(() => setSaved(false), 1800); }}>Save changes</button></div></div>;
}

export default App;
