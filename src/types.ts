export type View = "home" | "library" | "settings";

export interface AppSettings {
  onboardingComplete: boolean;
  provider: string;
  libraryPath: string;
  screenshotRetention: "all" | "failures" | "none";
  theme: "system" | "light" | "dark";
  shareScreenshotsWithPlanner: boolean;
  diagnosticLogging: boolean;
  plannerModels: Record<string, string>;
  plannerEfforts: Record<string, string>;
}

export interface ProviderEffortOption {
  id: string;
  description?: string;
}

export interface ProviderModelOption {
  id: string;
  displayName: string;
  defaultEffort?: string;
  efforts: ProviderEffortOption[];
}

export interface ProviderModelCatalog {
  provider: string;
  installed: boolean;
  defaultModel?: string;
  models: ProviderModelOption[];
  efforts: ProviderEffortOption[];
  effortMode?: string;
  source?: string;
  error?: string;
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

export interface Workflow {
  id: string;
  name: string;
  goal: string;
  version: string;
  createdAt: string;
  updatedAt: string;
  status: string;
  plannerProvider?: string;
  requiredApps: string[];
  steps: WorkflowStep[];
  completionEvidence?: string[];
  lastTypedText?: string;
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

export interface RunCheckpoint {
  runId: string;
  workflowId: string;
  nextStepIndex: number;
  status: string;
  error?: string | null;
  updatedAt: string;
}
