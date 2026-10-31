import { useEffect, useMemo, useState } from 'react';

import { createBridge } from './bridge/createBridge';
import type { EngineBridge } from './bridge/EngineBridge';
import { initialJobState, type JobState } from './bridge/jobReducer';
import { JobStream } from './bridge/JobStream';
import { initialRunState, type RunState } from './bridge/reducer';
import { RunStream } from './bridge/RunStream';
import type {
  AcceptanceSpec,
  HealthReport,
  JobRecord,
  JsonObject,
  JsonValue,
  MissionManifest,
  ModelKey,
  RunEvent,
  RunRecord,
  TaskRoute,
} from './bridge/types';
import { AcceptanceReview } from './components/AcceptanceReview';
import { ApprovalDialog, type ApprovalRequest } from './components/ApprovalDialog';
import { EvidenceInspector } from './components/EvidenceInspector';
import { MissionPicker } from './components/MissionPicker';
import { ModelDock } from './components/ModelDock';
import { RunHistory } from './components/RunHistory';
import { RunTimeline } from './components/RunTimeline';
import { TaskComposer } from './components/TaskComposer';
import { TodoTimeline } from './components/TodoTimeline';

interface Loaded {
  health: HealthReport;
  missions: MissionManifest[];
  runs: RunRecord[];
}

export default function WorkspaceView({ onOpenChat }: { onOpenChat?: () => void }) {
  const [bridge, setBridge] = useState<EngineBridge | null>(null);
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [selectedMission, setSelectedMission] = useState<string | null>(null);
  const [missionInputs, setMissionInputs] = useState<Record<string, string>>({});
  const [currentRunId, setCurrentRunId] = useState<string | null>(null);
  const [acceptance, setAcceptance] = useState<AcceptanceSpec | null>(null);
  const [runState, setRunState] = useState<RunState>(initialRunState);
  const [starting, setStarting] = useState(false);
  const [taskRoute, setTaskRoute] = useState<TaskRoute | null>(null);
  const [currentJobId, setCurrentJobId] = useState<string | null>(null);
  const [jobState, setJobState] = useState<JobState>(initialJobState);
  const [jobs, setJobs] = useState<JobRecord[]>([]);
  const [stoppingJob, setStoppingJob] = useState(false);
  const [deciding, setDeciding] = useState(false);
  const [retrying, setRetrying] = useState<string | null>(null);
  const [releasing, setReleasing] = useState<ModelKey | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      try {
        const engine = await createBridge();
        if (cancelled) {
          return;
        }
        setBridge(engine);
        const [health, missions, runs] = await Promise.all([
          engine.health(),
          engine.listMissions(),
          engine.listRuns(20),
        ]);
        if (!cancelled) {
          setLoaded({ health, missions, runs });
          setSelectedMission((current) => current ?? missions[0]?.name ?? null);
        }
      } catch (cause) {
        if (!cancelled) {
          setError(messageOf(cause));
        }
      }
    }

    void load();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!bridge || !currentRunId) {
      setRunState(initialRunState);
      return;
    }
    setRunState(initialRunState);
    const stream = new RunStream(bridge, currentRunId, setRunState);
    void stream.start().catch((cause: unknown) => {
      setActionError(messageOf(cause));
    });
    return () => {
      stream.close();
    };
  }, [bridge, currentRunId]);

  useEffect(() => {
    if (!bridge || !currentRunId) {
      setAcceptance(null);
      return;
    }
    let cancelled = false;
    bridge
      .getAcceptance(currentRunId)
      .then((spec) => {
        if (!cancelled) {
          setAcceptance(spec);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setAcceptance(null);
          setActionError(messageOf(cause));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [bridge, currentRunId]);

  const snapshot = runState.snapshot;
  const status = snapshot?.run.status ?? null;

  useEffect(() => {
    if (!bridge) {
      return;
    }
    let cancelled = false;
    bridge
      .listRuns(20)
      .then((runs) => {
        if (!cancelled) {
          setLoaded((current) => (current ? { ...current, runs } : current));
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [bridge, currentRunId, status]);

  useEffect(() => {
    if (!bridge || !currentJobId) {
      setJobState(initialJobState);
      return;
    }
    setJobState(initialJobState);
    const stream = new JobStream(bridge, currentJobId, setJobState);
    void stream.start().catch((cause: unknown) => {
      setActionError(messageOf(cause));
    });
    return () => {
      stream.close();
    };
  }, [bridge, currentJobId]);

  useEffect(() => {
    if (!bridge) {
      return;
    }
    let cancelled = false;
    bridge
      .listJobs(20)
      .then((entries) => {
        if (!cancelled) {
          setJobs(entries);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [bridge, currentJobId, jobState.job?.status, jobState.turns.length]);

  const approval = useMemo(() => approvalRequest(runState.events), [runState.events]);

  async function startRun() {
    if (!bridge || !loaded || !selectedMission) {
      return;
    }
    const mission = loaded.missions.find((entry) => entry.name === selectedMission);
    if (!mission) {
      return;
    }
    setStarting(true);
    setActionError(null);
    try {
      const inputs: Record<string, JsonValue> = {};
      for (const [key, spec] of Object.entries(mission.inputs)) {
        const raw = (missionInputs[key] ?? '').trim();
        if (raw.length === 0) {
          continue;
        }
        inputs[key] = spec.kind === 'json' ? parseJson(raw) : raw;
      }
      const runId = await bridge.startRun({ mission: mission.name, inputs });
      setCurrentJobId(null);
      setCurrentRunId(runId);
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setStarting(false);
    }
  }

  async function startJob(
    goal: string,
    deliverable: string,
    minutes: number,
    maxTurns: number,
  ) {
    if (!bridge) {
      return;
    }
    setStarting(true);
    setActionError(null);
    try {
      const result = await bridge.startJob({
        goal,
        deliverable: deliverable || null,
        minutes,
        max_turns: maxTurns,
      });
      setTaskRoute(result.route);
      setCurrentRunId(null);
      setJobState(initialJobState);
      setCurrentJobId(result.job_id);
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setStarting(false);
    }
  }

  async function stopJob(jobId: string) {
    if (!bridge) {
      return;
    }
    setStoppingJob(true);
    setActionError(null);
    try {
      await bridge.cancelJob(jobId);
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setStoppingJob(false);
    }
  }

  function openJob(jobId: string) {
    setCurrentRunId(null);
    setJobState(initialJobState);
    setCurrentJobId(jobId);
  }

  async function approvePlan(hash: string) {
    if (!bridge || !currentRunId) {
      return;
    }
    setDeciding(true);
    setActionError(null);
    try {
      await bridge.approvePlan(currentRunId, hash);
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setDeciding(false);
    }
  }

  async function rejectPlan() {
    if (!bridge || !currentRunId) {
      return;
    }
    setDeciding(true);
    setActionError(null);
    try {
      await bridge.cancelRun(currentRunId);
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setDeciding(false);
    }
  }

  async function cancelRun(runId: string) {
    if (!bridge) {
      return;
    }
    setActionError(null);
    try {
      await bridge.cancelRun(runId);
    } catch (cause) {
      setActionError(messageOf(cause));
    }
  }

  async function retryRun(runId: string) {
    if (!bridge) {
      return;
    }
    setRetrying(runId);
    setActionError(null);
    try {
      const next = await bridge.retryRun(runId);
      setCurrentRunId(next);
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setRetrying(null);
    }
  }

  async function decide(approvalId: string, allow: boolean) {
    if (!bridge) {
      return;
    }
    setDeciding(true);
    setActionError(null);
    try {
      await bridge.decideApproval(approvalId, allow);
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setDeciding(false);
    }
  }

  async function release(modelKey: ModelKey) {
    if (!bridge) {
      return;
    }
    setReleasing(modelKey);
    setActionError(null);
    try {
      await bridge.releaseModel(modelKey);
      const models = await bridge.listModels();
      setLoaded((current) =>
        current ? { ...current, health: { ...current.health, models } } : current,
      );
    } catch (cause) {
      setActionError(messageOf(cause));
    } finally {
      setReleasing(null);
    }
  }

  async function chooseFile(mission: string, inputKey: string) {
    if (!bridge) {
      return;
    }
    setActionError(null);
    try {
      const files = await bridge.chooseInputFiles(mission, inputKey);
      if (files.length > 0) {
        setMissionInputs((current) => ({ ...current, [inputKey]: files[0] }));
      }
    } catch (cause) {
      setActionError(messageOf(cause));
    }
  }

  return (
    <main className="shell">
      <header className="masthead">
        <div>
          <p className="eyebrow">Everything stays on this computer</p>
          <h1>Tasks</h1>
        </div>
        <div className="badges">
          {onOpenChat ? (
            <button type="button" className="small" onClick={onOpenChat}>
              Back to chat
            </button>
          ) : null}
          <span className="badge badge-local">On this computer</span>
          {bridge?.kind === 'demo' ? <span className="badge badge-demo">Demo data</span> : null}
        </div>
      </header>

      {error ? (
        <section className="panel panel-error" role="alert">
          <h2>The assistant isn’t running</h2>
          <p>{error}</p>
          <p className="muted">Open the desktop app to connect to the local engine.</p>
          {import.meta.env.DEV ? (
            <p className="muted">
              In development, append <code>?demo=1</code> to view labelled fixtures.
            </p>
          ) : null}
        </section>
      ) : null}

      {actionError ? (
        <section className="panel panel-error" role="alert">
          <h2>Action failed</h2>
          <p>{actionError}</p>
        </section>
      ) : null}

      {!loaded && !error ? (
        <section className="panel">
          <p className="muted">Getting things ready…</p>
        </section>
      ) : null}

      {loaded ? (
        <>
          <TaskComposer
            busy={starting}
            demo={bridge?.kind === 'demo'}
            route={taskRoute}
            onStart={(goal, deliverable, minutes, maxTurns) =>
              void startJob(goal, deliverable, minutes, maxTurns)
            }
          />

          {jobState.job ? (
            <TodoTimeline
              job={jobState.job}
              todos={jobState.todos}
              turns={jobState.turns}
              jobs={jobs}
              cancelling={stoppingJob}
              recovering={jobState.phase === 'recovering'}
              onSelect={openJob}
              onCancel={(jobId) => void stopJob(jobId)}
            />
          ) : null}

          <MissionPicker
            missions={loaded.missions}
            selected={selectedMission}
            inputs={missionInputs}
            busy={starting}
            demo={bridge?.kind === 'demo'}
            onSelect={(name) => {
              setSelectedMission(name);
              setMissionInputs({});
            }}
            onInputChange={(key, value) =>
              setMissionInputs((current) => ({ ...current, [key]: value }))
            }
            onChooseFile={(mission, key) => void chooseFile(mission, key)}
            onStart={() => void startRun()}
          />

          <ModelDock
            models={loaded.health.models}
            llama={loaded.health.llama}
            version={loaded.health.version}
            dataDir={loaded.health.data_dir}
            releasing={releasing}
            onRelease={(key) => void release(key)}
          />

          {snapshot ? (
            <>
              {runState.phase === 'recovering' ? (
                <p className="notice" role="status">
                  Recovering journal after an event gap. Reading the authoritative snapshot
                  before applying new events.
                </p>
              ) : null}
              <RunTimeline
                snapshot={snapshot}
                events={runState.events}
                cancelling={status === 'cancelling'}
                retrying={retrying === snapshot.run.id}
                onCancel={(runId) => void cancelRun(runId)}
                onRetry={(runId) => void retryRun(runId)}
              />
              {acceptance && status === 'awaiting_plan_approval' ? (
                <AcceptanceReview
                  acceptance={acceptance}
                  acceptanceHash={snapshot.run.acceptance_hash}
                  expectedHash={snapshot.run.acceptance_hash}
                  busy={deciding}
                  onApprove={(hash) => void approvePlan(hash)}
                  onReject={() => void rejectPlan()}
                />
              ) : null}
              <EvidenceInspector
                snapshot={snapshot}
                events={runState.events}
                final={status === 'passed'}
              />
            </>
          ) : (
            <section className="panel">
              <h2>Nothing open yet</h2>
              <p className="muted">
                Start a task above, or pick one from history. Each step and check is kept here so
                you can see exactly what happened.
              </p>
            </section>
          )}

          <RunHistory
            runs={loaded.runs}
            activeRunId={currentRunId}
            busyRunId={retrying}
            onSelect={(runId) => setCurrentRunId(runId)}
            onRetry={(runId) => void retryRun(runId)}
          />
        </>
      ) : null}

      <ApprovalDialog request={approval} busy={deciding} onDecide={(id, allow) => void decide(id, allow)} />
    </main>
  );
}

function approvalRequest(events: readonly RunEvent[]): ApprovalRequest | null {
  let pending: ApprovalRequest | null = null;
  for (const event of events) {
    if (event.kind === 'approval.requested') {
      pending = toApproval(event.payload);
    } else if (event.kind === 'approval.resolved' && pending !== null) {
      const resolved = stringField(event.payload, 'approval_id');
      if (resolved === null || resolved === pending.id) {
        pending = null;
      }
    }
  }
  return pending;
}

function toApproval(payload: JsonObject): ApprovalRequest | null {
  const id = stringField(payload, 'approval_id') ?? stringField(payload, 'id');
  if (!id) {
    return null;
  }
  return {
    id,
    run_id: stringField(payload, 'run_id') ?? '',
    step_index: numberField(payload, 'step_index') ?? 0,
    tool: (stringField(payload, 'tool') ?? 'write_file') as ApprovalRequest['tool'],
    summary: stringField(payload, 'summary') ?? 'The step requests approval.',
    detail: stringField(payload, 'detail') ?? '',
    risk: riskField(payload, 'risk'),
    expires_at: numberField(payload, 'expires_at'),
  };
}

function riskField(payload: JsonObject, key: string): ApprovalRequest['risk'] {
  const value = payload[key];
  return value === 'low' || value === 'high' ? value : 'medium';
}

function stringField(payload: JsonObject, key: string): string | null {
  const value = payload[key];
  return typeof value === 'string' ? value : null;
}

function numberField(payload: JsonObject, key: string): number | null {
  const value = payload[key];
  return typeof value === 'number' ? value : null;
}

function parseJson(raw: string): JsonValue {
  return JSON.parse(raw) as JsonValue;
}

function messageOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}
