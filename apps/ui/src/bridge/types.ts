/**
 * TypeScript mirrors of the harness contracts exposed through Tauri commands.
 *
 * These are view contracts, not a second source of truth: the Rust engine
 * freezes acceptance, owns run state, and returns these shapes. The UI never
 * derives a pass from prose and never mutates a run locally.
 */

export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };
export type JsonObject = { [key: string]: JsonValue };

export type RunId = string;
export type AttemptId = string;
export type ArtifactId = string;

export type ModelKey = 'text' | 'vision' | 'audio' | 'd1';

export const RUN_STATUSES = [
  'created',
  'planning',
  'awaiting_plan_approval',
  'running',
  'final_verifying',
  'passed',
  'awaiting_user',
  'cancelling',
  'cancelled',
  'incomplete',
  'interrupted',
] as const;
export type RunStatus = (typeof RUN_STATUSES)[number];

export const STEP_STATUSES = [
  'pending',
  'running',
  'verifying',
  'retrying',
  'replanning',
  'passed',
  'blocked',
  'cancelled',
] as const;
export type StepStatus = (typeof STEP_STATUSES)[number];

export const ATTEMPT_STATUSES = [
  'proposed',
  'guarded',
  'executing',
  'verifying',
  'passed',
  'failed',
  'inconclusive',
  'aborted',
  'interrupted',
] as const;
export type AttemptStatus = (typeof ATTEMPT_STATUSES)[number];

export type CheckStatus = 'pass' | 'fail' | 'error';
export type ToolStatus = 'succeeded' | 'failed';

export type ToolName =
  | 'list_files'
  | 'read_file'
  | 'write_file'
  | 'edit_file'
  | 'search_files'
  | 'related_files'
  | 'look'
  | 'transcribe'
  | 'speak';

// ---------------------------------------------------------------- missions

export type InputKind = 'string' | 'image' | 'audio' | 'json' | 'file';

export interface InputSpec {
  kind: InputKind;
  required: boolean;
  schema: JsonValue | null;
}

export interface CommandTemplateDef {
  id: string;
  executable: string;
  args: string[];
}

export interface RequiredArtifact {
  path: string;
  non_empty: boolean;
}

export interface OutputContract {
  schema: JsonValue | null;
  required_artifacts: RequiredArtifact[];
}

export interface MissionLimits {
  max_steps: number;
  max_attempts: number;
}

export type SemanticMode = 'human_review_only' | 'automatic';

export interface SemanticQualification {
  holdout_sha256: string;
  runs: number;
  false_completions: number;
  evaluated_at: string;
}

/** The mission's frozen semantic capability. Automatic passing needs a
 * validated holdout qualification; the engine refuses to start any mission
 * whose semantic criteria are human-review-only. */
export interface SemanticPolicy {
  mode: SemanticMode;
  min_confidence_bps: number;
  qualification: SemanticQualification | null;
  reason: string | null;
}

export interface MissionManifest {
  schema_version: number;
  name: string;
  version: number;
  title: string;
  description: string;
  inputs: Record<string, InputSpec>;
  tools: ToolName[];
  checks: string[];
  command_templates: CommandTemplateDef[];
  output: OutputContract;
  limits: MissionLimits;
  semantic?: SemanticPolicy | null;
}

// -------------------------------------------------------------- acceptance

export interface CheckSpec {
  kind: string;
  [argument: string]: JsonValue;
}

export interface PlannedStep {
  index: number;
  title: string;
  criterion: string;
  checks: CheckSpec[];
  tool: ToolName | null;
}

export interface InputArtifact {
  id: ArtifactId;
  sha256: string;
}

export interface AcceptanceSpec {
  schema_version: number;
  mission: string;
  mission_version: number;
  goal: string;
  inputs: Record<string, InputArtifact>;
  steps: PlannedStep[];
  output: OutputContract;
  allowed_tools: ToolName[];
  allowed_checks: string[];
  max_steps: number;
  max_attempts: number;
}

export interface StrategyAction {
  step_index: number;
  tool: ToolName;
  args: JsonObject;
}

export interface ExecutionStrategy {
  acceptance_hash: string;
  actions: StrategyAction[];
}

// --------------------------------------------------------------- run state

export interface RunRecord {
  id: RunId;
  mission: string;
  mission_version: number;
  acceptance_hash: string;
  goal: string;
  status: RunStatus;
  created_at: number;
  updated_at: number;
  parent_run_id: RunId | null;
}

export interface AttemptSnapshot {
  id: AttemptId;
  strategy: number;
  status: AttemptStatus;
  action: JsonValue;
  model: string;
  started_at: number;
  finished_at: number | null;
  error: string | null;
}

export interface StepSnapshot {
  index: number;
  title: string;
  criterion: string;
  status: StepStatus;
  attempts: AttemptSnapshot[];
}

export interface RunSnapshot {
  run: RunRecord;
  steps: StepSnapshot[];
}

// ---------------------------------------------------------------- evidence

export interface ContentDigest {
  sha256: string;
  size: number;
  media_type: string;
}

export interface SemanticEvidence {
  chain: string;
  model: string;
  prompt: string;
  response: string;
  label: string;
  confidence_bps: number;
  threshold_bps: number;
  pointers: string[];
  unsupported: string[];
}

export interface CheckEvidence {
  source: string;
  attempt: AttemptId | null;
  digest: ContentDigest | null;
  detail: string;
  /** Present only on semantic judgements; deterministic evidence omits it. */
  semantic?: SemanticEvidence | null;
}

export interface CheckResult {
  kind: string;
  acceptance_hash: string;
  step_index: number;
  attempt: AttemptId;
  status: CheckStatus;
  message: string;
  evidence: CheckEvidence[];
}

/** Rust serializes `std::time::Duration` as whole seconds and nanoseconds. */
export interface DurationParts {
  secs: number;
  nanos: number;
}

export interface ToolResult {
  attempt: AttemptId;
  call_id: string;
  tool: ToolName;
  status: ToolStatus;
  exit_code: number | null;
  output: string;
  artifacts: ContentDigest[];
  duration: DurationParts;
}

// ------------------------------------------------------------------ events

export const RUN_EVENT_KINDS = [
  'run.created',
  'run.started',
  'run.status',
  'run.planned',
  'plan.ready',
  'step.started',
  'step.replanned',
  'step.passed',
  'step.failed',
  'step.inconclusive',
  'step.blocked',
  'attempt.started',
  'attempt.failed',
  'tool.result',
  'check.result',
  'recovery.decision',
  'approval.requested',
  'approval.resolved',
  'model.loading',
  'model.ready',
  'model.evicted',
  'run.cancelling',
  'run.final_verifying',
  'run.interrupted',
  'run.finished',
] as const;
export type RunEventKind = (typeof RUN_EVENT_KINDS)[number];

/**
 * One durable, ordered journal entry. `seq` is the gap detector: the UI only
 * applies `lastSeq + 1` and refetches from the store when an entry is missing.
 */
export interface RunEvent {
  run_id: RunId;
  seq: number;
  kind: RunEventKind;
  payload: JsonObject;
  at: number;
}

// ------------------------------------------------------------------ health

export type ModelState = 'absent' | 'loading' | 'ready' | 'evicted' | 'error';

export interface ModelStatus {
  key: ModelKey;
  model: string;
  url: string;
  state: ModelState;
  memory_bytes: number;
  error: string | null;
}

export interface LlamaRuntimeStatus {
  executable: string | null;
  build: number;
  release: string;
  pinned: boolean;
}

export interface HealthReport {
  version: string;
  local_only: true;
  data_dir: string;
  llama: LlamaRuntimeStatus;
  models: ModelStatus[];
  /** The local voice used for spoken replies. LFM3-Audio-2B renders the WAV
   * when the runtime and weights are present; otherwise the OS voice is used. */
  voice_output?: VoiceOutputStatus;
}

export interface VoiceOutputStatus {
  available: boolean;
  backend: string | null;
  /** The model that renders speech, when the backend is LFM3. */
  model?: string | null;
  lfm3_tts_weights: boolean;
  note: string;
}

export interface StartRunRequest {
  mission: string;
  inputs: Record<string, JsonValue>;
}

// -------------------------------------------------------------------- chat

export type ChatRole = 'system' | 'user' | 'assistant' | 'tool';
export type ChatMessageStatus = 'complete' | 'streaming' | 'cancelled' | 'error';
export type ChatActionStatus =
  | 'pending'
  | 'approved'
  | 'denied'
  | 'applied'
  | 'failed'
  | 'expired';

export interface ChatSession {
  id: string;
  title: string;
  created_at: number;
  updated_at: number;
}

export interface ChatMessageRecord {
  id: string;
  session_id: string;
  seq: number;
  role: ChatRole;
  content: string;
  status: ChatMessageStatus;
  meta: JsonObject;
  created_at: number;
}

export interface ChatActionRecord {
  id: string;
  session_id: string;
  message_id: string;
  tool: string;
  arguments: JsonObject;
  summary: string;
  status: ChatActionStatus;
  created_at: number;
  decided_at: number | null;
}

export interface ChatSessionView {
  session: ChatSession;
  messages: ChatMessageRecord[];
  actions: ChatActionRecord[];
}

/** Flat live events from `lfm3:chat-event`. */
export type ChatEvent =
  | { session_id: string; kind: 'user_message'; message: ChatMessageRecord }
  | { session_id: string; kind: 'session_updated'; session: ChatSession }
  | { session_id: string; kind: 'assistant_start'; message: ChatMessageRecord }
  | { session_id: string; kind: 'assistant_delta'; message_id: string; delta: string }
  | { session_id: string; kind: 'assistant_done'; message: ChatMessageRecord }
  | { session_id: string; kind: 'route'; route: ChatRoute }
  | { session_id: string; kind: 'action_requested'; action: ChatActionRecord }
  | { session_id: string; kind: 'action_resolved'; action: ChatActionRecord }
  | { session_id: string; kind: 'tool_result'; message: ChatMessageRecord }
  | { session_id: string; kind: 'status'; state: string; detail: string | null }
  | { session_id: string; kind: 'error'; message: string }
  | { session_id: string; kind: 'done' };

export type ChatIntent =
  | 'general'
  | 'explore'
  | 'summarize'
  | 'related'
  | 'report'
  | 'vision'
  | 'voice';

export interface ChatRouteRole {
  role: string;
  model: ModelKey;
  capability: string;
  used: boolean;
}

/** The decision model's route for one turn, shown above the answer. */
export interface ChatRoute {
  intent: ChatIntent;
  source: 'decision' | 'heuristic';
  confidence_bps: number;
  reason: string;
  model: string | null;
  roles: ChatRouteRole[];
}

/** One locally synthesized spoken reply. */
export interface SpeechReply {
  audio_base64: string;
  format: string;
  backend: string;
  note: string;
}

// -------------------------------------------------------------- general task

/** One model role and whether the routed task will consult it. */
export interface TaskRouteRole {
  role: 'plan' | 'act' | 'observe' | 'hear' | 'judge';
  model: ModelKey;
  capability: string;
  used: boolean;
  reason: string | null;
}

/** The capability decision the engine made for one stated task. */
export interface TaskRoute {
  roles: TaskRouteRole[];
  images: string[];
  audio: string[];
  tools: ToolName[];
}

// -------------------------------------------------------------------- jobs

/**
 * A job is a model-driven turn loop, not a verified run. It has no frozen
 * acceptance contract, so it can never be reported as passed mission work.
 */
export type JobStatus = 'running' | 'passed' | 'failed' | 'cancelled' | 'interrupted';

/** Measured proof that the required output exists; never a model claim. */
export interface JobDeliverable {
  path: string;
  bytes: number;
  digest: string;
}

export interface JobRecord {
  id: string;
  goal: string;
  deliverable: string | null;
  status: JobStatus;
  /** Why the loop stopped, in the engine's words. */
  stopped: string;
  model: string;
  max_turns: number;
  minutes: number;
  deliverable_proof: JobDeliverable | null;
  created_at: number;
  updated_at: number;
  finished_at: number | null;
}

/** A deterministic check a plan step attaches to itself. */
export interface JobTodoCheck {
  kind: 'file_exists' | 'dir_exists' | 'content_matches';
  path: string;
  contains?: string | null;
}

/** One item of the acting model's own plan, as it last wrote it. */
export interface JobTodo {
  text: string;
  done: boolean;
  check?: JobTodoCheck | null;
}

export interface JobCall {
  tool: string;
  arguments: JsonValue;
  ok: boolean;
  output: string;
}

export interface JobTurn {
  index: number;
  /** plan, work, or review; absent on journals from older engines. */
  phase?: string;
  note: string;
  calls: JobCall[];
}

/** The job plus the todo list and turns folded from its own journal. */
export interface JobView {
  job: JobRecord;
  todos: JobTodo[];
  turns: JobTurn[];
}

export const JOB_EVENT_KINDS = [
  'job.started',
  'loop.turn',
  'loop.todos',
  'job.finished',
  'job.interrupted',
] as const;
export type JobEventKind = (typeof JOB_EVENT_KINDS)[number];

/** One ordered job journal entry; `seq` is the gap detector. */
export interface JobEvent {
  job_id: string;
  seq: number;
  kind: JobEventKind;
  payload: JsonObject;
  at: number;
}

export interface StartJobRequest {
  goal: string;
  deliverable?: string | null;
  /** Whole-job budget in minutes. */
  minutes?: number;
  /** Hard cap on model turns. */
  max_turns?: number;
}

export interface StartJobResult {
  job_id: string;
  job: JobRecord;
  route: TaskRoute;
}
