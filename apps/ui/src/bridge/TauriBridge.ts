import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

import {
  CHAT_EVENT_CHANNEL,
  ENGINE_COMMANDS,
  JOB_EVENT_CHANNEL,
  RUN_EVENT_CHANNEL,
  type ChatEventListener,
  type EngineBridge,
  type JobEventListener,
  type RunEventListener,
  type Unsubscribe,
} from './EngineBridge';
import type {
  AcceptanceSpec,
  ChatEvent,
  ChatMessageRecord,
  ChatSession,
  ChatSessionView,
  HealthReport,
  MissionManifest,
  ModelKey,
  ModelStatus,
  RunEvent,
  RunId,
  RunRecord,
  RunSnapshot,
  SpeechReply,
  JobEvent,
  JobRecord,
  JobView,
  StartJobRequest,
  StartJobResult,
  StartRunRequest,
} from './types';

/** Production bridge: typed Tauri commands and durable journal events. */
export class TauriBridge implements EngineBridge {
  readonly kind = 'tauri' as const;

  health(): Promise<HealthReport> {
    return invoke<HealthReport>(ENGINE_COMMANDS.health);
  }

  listMissions(): Promise<MissionManifest[]> {
    return invoke<MissionManifest[]>(ENGINE_COMMANDS.listMissions);
  }

  listRuns(limit: number): Promise<RunRecord[]> {
    return invoke<RunRecord[]>(ENGINE_COMMANDS.listRuns, { limit });
  }

  startRun(request: StartRunRequest): Promise<RunId> {
    return invoke<RunId>(ENGINE_COMMANDS.startRun, {
      mission: request.mission,
      inputs: request.inputs,
    });
  }

  listJobs(limit: number): Promise<JobRecord[]> {
    return invoke<JobRecord[]>(ENGINE_COMMANDS.listJobs, { limit });
  }

  startJob(request: StartJobRequest): Promise<StartJobResult> {
    return invoke<StartJobResult>(ENGINE_COMMANDS.startJob, {
      goal: request.goal,
      deliverable: request.deliverable ?? null,
      minutes: request.minutes ?? null,
      maxTurns: request.max_turns ?? null,
    });
  }

  getJob(jobId: string): Promise<JobView> {
    return invoke<JobView>(ENGINE_COMMANDS.getJob, { jobId });
  }

  getJobEvents(jobId: string, after: number): Promise<JobEvent[]> {
    return invoke<JobEvent[]>(ENGINE_COMMANDS.getJobEvents, { jobId, after });
  }

  cancelJob(jobId: string): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.cancelJob, { jobId });
  }

  async subscribeJob(listener: JobEventListener): Promise<Unsubscribe> {
    return listen<JobEvent>(JOB_EVENT_CHANNEL, (event) => listener(event.payload));
  }

  getRunSnapshot(runId: RunId): Promise<RunSnapshot> {
    return invoke<RunSnapshot>(ENGINE_COMMANDS.getRunSnapshot, { runId });
  }

  getAcceptance(runId: RunId): Promise<AcceptanceSpec> {
    return invoke<AcceptanceSpec>(ENGINE_COMMANDS.getAcceptance, { runId });
  }

  getRunEvents(runId: RunId, after: number): Promise<RunEvent[]> {
    return invoke<RunEvent[]>(ENGINE_COMMANDS.getRunEvents, { runId, after });
  }

  approvePlan(runId: RunId, acceptanceHash: string): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.approvePlan, { runId, acceptanceHash });
  }

  decideApproval(approvalId: string, allow: boolean): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.decideApproval, { approvalId, allow });
  }

  cancelRun(runId: RunId): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.cancelRun, { runId });
  }

  retryRun(runId: RunId): Promise<RunId> {
    return invoke<RunId>(ENGINE_COMMANDS.retryRun, { runId });
  }

  listModels(): Promise<ModelStatus[]> {
    return invoke<ModelStatus[]>(ENGINE_COMMANDS.listModels);
  }

  releaseModel(modelKey: ModelKey): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.releaseModel, { modelKey });
  }

  chooseInputFiles(mission: string, inputKey: string): Promise<string[]> {
    return invoke<string[]>(ENGINE_COMMANDS.chooseInputFiles, { mission, inputKey });
  }

  openArtifact(artifactId: string): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.openArtifact, { artifactId });
  }

  async subscribe(runId: RunId, listener: RunEventListener): Promise<Unsubscribe> {
    const unlisten = await listen<RunEvent>(RUN_EVENT_CHANNEL, (event) => {
      if (event.payload.run_id === runId) {
        listener(event.payload);
      }
    });
    return unlisten;
  }

  listChatSessions(limit: number): Promise<ChatSession[]> {
    return invoke<ChatSession[]>(ENGINE_COMMANDS.listChatSessions, { limit });
  }

  createChatSession(title?: string): Promise<ChatSession> {
    return invoke<ChatSession>(ENGINE_COMMANDS.createChatSession, { title: title ?? null });
  }

  getChatSession(sessionId: string): Promise<ChatSessionView> {
    return invoke<ChatSessionView>(ENGINE_COMMANDS.getChatSession, { sessionId });
  }

  renameChatSession(sessionId: string, title: string): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.renameChatSession, { sessionId, title });
  }

  deleteChatSession(sessionId: string): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.deleteChatSession, { sessionId });
  }

  sendChatMessage(sessionId: string, content: string): Promise<ChatMessageRecord> {
    return invoke<ChatMessageRecord>(ENGINE_COMMANDS.sendChatMessage, { sessionId, content });
  }

  cancelChat(sessionId: string): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.cancelChat, { sessionId });
  }

  decideChatAction(actionId: string, allow: boolean): Promise<void> {
    return invoke<void>(ENGINE_COMMANDS.decideChatAction, { actionId, allow });
  }

  transcribeVoice(audioBase64: string, format: string): Promise<string> {
    return invoke<string>(ENGINE_COMMANDS.transcribeVoice, { audioBase64, format });
  }

  synthesizeSpeech(text: string): Promise<SpeechReply> {
    return invoke<SpeechReply>(ENGINE_COMMANDS.synthesizeSpeech, { text });
  }

  attachImage(name: string, dataBase64: string): Promise<string> {
    return invoke<string>(ENGINE_COMMANDS.attachImage, { name, dataBase64 });
  }

  async subscribeChat(listener: ChatEventListener): Promise<Unsubscribe> {
    return listen<ChatEvent>(CHAT_EVENT_CHANNEL, (event) => listener(event.payload));
  }
}
