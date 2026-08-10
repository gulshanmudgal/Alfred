export type View = "home" | "workflows" | "runs" | "schedules" | "settings";

export interface AppSettings {
  onboardingComplete: boolean;
  provider: string;
  libraryPath: string;
  screenshotRetention: "all" | "failures" | "none";
  theme: "system" | "light" | "dark";
  shareScreenshotsWithPlanner: boolean;
}

export interface SystemInfo {
  os: string;
  architecture: string;
  defaultLibraryPath: string;
  nativeHost: string;
}

export interface ProviderStatus {
  id: string;
  name: string;
  command: string;
  installed: boolean;
  version?: string;
  credentialStored: boolean;
}

export interface StepCondition {
  automationId?: string;
  name?: string;
  controlType?: string;
  urlContains?: string;
  absent?: boolean;
}

export interface WorkflowStep {
  id: string;
  title: string;
  kind: string;
  effect: string;
  application?: string;
  intent?: string;
  targetLabel?: string;
  payload?: Record<string, unknown>;
  timeoutMs: number;
  retries: number;
  waitFor?: StepCondition;
  expect?: StepCondition;
  saveAs?: string;
}

export interface PermissionGrant {
  id: string;
  application: string;
  allowedEffects: string[];
  allowedIntents: string[];
  enabled: boolean;
  createdAt: string;
}

export interface WorkflowSchedule {
  id: string;
  workflowId: string;
  workflowName: string;
  hour: number;
  minute: number;
  days: number[];
  enabled: boolean;
  lastTriggeredKey?: string;
  createdAt: string;
}

export interface ProviderEvent {
  sessionId: string;
  provider: string;
  stream: string;
  line: string;
  status: string;
  timestamp: string;
}

export interface Workflow {
  id: string;
  name: string;
  goal: string;
  version: string;
  createdAt: string;
  updatedAt: string;
  status: string;
  requiredApps: string[];
  steps: WorkflowStep[];
}

export interface RunEvent {
  runId: string;
  sequence: number;
  stepId: string;
  title: string;
  detail: string;
  application: string;
  status: string;
  progress: number;
  evidenceDataUrl?: string;
  timestamp: string;
}
