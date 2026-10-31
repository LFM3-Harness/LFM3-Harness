import type {
  AcceptanceSpec,
  ChatEvent,
  ChatMessageRecord,
  ChatSession,
  ChatSessionView,
  DownloadEvent,
  HealthReport,
  MissionManifest,
  ModelCatalog,
  ModelKey,
  ModelStatus,
  RunEvent,
  RunId,
  RunRecord,
  RunSnapshot,
  JobEvent,
  JobRecord,
  JobView,
  SpeechReply,
  StartJobRequest,
  StartJobResult,
  StartRunRequest,
} from './types';

/**
 * The exact command surface from PLAN.md §17. Command names are shared with
 * the Tauri layer so a missing command fails visibly instead of falling back
 * to an HTTP side channel.
 */
export const ENGINE_COMMANDS = {
  health: 'health',
  listMissions: 'list_missions',
  listRuns: 'list_runs',
  startRun: 'start_run',
  listJobs: 'list_jobs',
  startJob: 'start_job',
  getJob: 'get_job',
  getJobEvents: 'get_job_events',
  cancelJob: 'cancel_job',
  getAcceptance: 'get_acceptance',
  getRunSnapshot: 'get_run_snapshot',
  getRunEvents: 'get_run_events',
  approvePlan: 'approve_plan',
  decideApproval: 'decide_approval',
  cancelRun: 'cancel_run',
  retryRun: 'retry_run',
  listModels: 'list_models',
  releaseModel: 'release_model',
  chooseWorkspace: 'choose_workspace',
  modelCatalog: 'model_catalog',
  downloadModel: 'download_model',
  cancelDownload: 'cancel_download',
  reloadModels: 'reload_models',
  chooseInputFiles: 'choose_input_files',
  openArtifact: 'open_artifact',
  listChatSessions: 'list_chat_sessions',
  createChatSession: 'create_chat_session',
  getChatSession: 'get_chat_session',
  renameChatSession: 'rename_chat_session',
  deleteChatSession: 'delete_chat_session',
  sendChatMessage: 'send_chat_message',
  cancelChat: 'cancel_chat',
  decideChatAction: 'decide_chat_action',
  transcribeVoice: 'transcribe_voice',
  synthesizeSpeech: 'synthesize_speech',
  attachImage: 'attach_image',
} as const;

/** The single Tauri event channel used for durable run events. */
export const RUN_EVENT_CHANNEL = 'lfm3:run-event';

/** Live chat events: streaming deltas, status, and approval cards. */
export const CHAT_EVENT_CHANNEL = 'lfm3:chat-event';

/** Live job events: journaled turns, todos, and the terminal outcome. */
export const JOB_EVENT_CHANNEL = 'lfm3:job-event';

/** Live download progress for explicit, user-initiated model downloads. */
export const DOWNLOAD_EVENT_CHANNEL = 'lfm3:download-event';

export type Unsubscribe = () => void;
export type RunEventListener = (event: RunEvent) => void;
export type ChatEventListener = (event: ChatEvent) => void;
export type JobEventListener = (event: JobEvent) => void;
export type DownloadEventListener = (event: DownloadEvent) => void;

/** Read and live-event access used by the reducer and stream controller. */
export interface RunReadSource {
  getRunSnapshot(runId: RunId): Promise<RunSnapshot>;
  getRunEvents(runId: RunId, after: number): Promise<RunEvent[]>;
  subscribe(runId: RunId, listener: RunEventListener): Promise<Unsubscribe>;
}

/** Read and live-event access for one job's turn stream. */
export interface JobReadSource {
  getJob(jobId: string): Promise<JobView>;
  getJobEvents(jobId: string, after: number): Promise<JobEvent[]>;
  subscribeJob(listener: JobEventListener): Promise<Unsubscribe>;
}

export interface EngineBridge extends RunReadSource, JobReadSource {
  /** `demo` is visible in the UI and cannot be built into the packaged app. */
  readonly kind: 'tauri' | 'demo';

  health(): Promise<HealthReport>;
  listMissions(): Promise<MissionManifest[]>;
  listRuns(limit: number): Promise<RunRecord[]>;
  startRun(request: StartRunRequest): Promise<RunId>;
  listJobs(limit: number): Promise<JobRecord[]>;
  /** Starts a stated job as a model-driven turn loop. It runs immediately. */
  startJob(request: StartJobRequest): Promise<StartJobResult>;
  cancelJob(jobId: string): Promise<void>;
  getAcceptance(runId: RunId): Promise<AcceptanceSpec>;
  approvePlan(runId: RunId, acceptanceHash: string): Promise<void>;
  decideApproval(approvalId: string, allow: boolean): Promise<void>;
  cancelRun(runId: RunId): Promise<void>;
  retryRun(runId: RunId): Promise<RunId>;
  listModels(): Promise<ModelStatus[]>;
  releaseModel(modelKey: ModelKey): Promise<void>;
  /** Opens the native folder picker; `null` when the person cancels. */
  chooseWorkspace(): Promise<string | null>;
  /** What can be downloaded, what is on disk, and which download is live. */
  modelCatalog(): Promise<ModelCatalog>;
  /** Starts one explicit download of a missing role. Refused while busy. */
  downloadModel(modelKey: ModelKey): Promise<void>;
  cancelDownload(modelKey: ModelKey): Promise<void>;
  /** Swaps installed weights into the running engine when it is idle. */
  reloadModels(): Promise<ModelStatus[]>;
  chooseInputFiles(mission: string, inputKey: string): Promise<string[]>;
  openArtifact(artifactId: string): Promise<void>;

  listChatSessions(limit: number): Promise<ChatSession[]>;
  createChatSession(title?: string): Promise<ChatSession>;
  getChatSession(sessionId: string): Promise<ChatSessionView>;
  renameChatSession(sessionId: string, title: string): Promise<void>;
  deleteChatSession(sessionId: string): Promise<void>;
  sendChatMessage(sessionId: string, content: string): Promise<ChatMessageRecord>;
  cancelChat(sessionId: string): Promise<void>;
  decideChatAction(actionId: string, allow: boolean): Promise<void>;
  transcribeVoice(audioBase64: string, format: string): Promise<string>;
  synthesizeSpeech(text: string): Promise<SpeechReply>;
  attachImage(name: string, dataBase64: string): Promise<string>;
  subscribeChat(listener: ChatEventListener): Promise<Unsubscribe>;
  subscribeDownloads(listener: DownloadEventListener): Promise<Unsubscribe>;
}
