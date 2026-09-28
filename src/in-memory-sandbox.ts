import type {
  CancelMessage,
  HostToWorkerMessage,
  SandboxProtocolVersion,
  WorkerToHostMessage,
} from "./sandbox-protocol.js";
import { negotiateProtocolVersion } from "./sandbox-protocol.js";
import { SandboxFailure, type JobSandboxSpec, type SandboxJob, type SandboxRunner } from "./sandbox.js";

interface ActiveRun {
  id: string;
  terminal: boolean;
  resolve(output: string): void;
  reject(error: Error): void;
  timer?: ReturnType<typeof setTimeout>;
}

/**
 * Controllable protocol adapter for tests. Tests inspect `commands` and inject
 * worker messages with `receive`, without spawning a process or container.
 */
export class InMemorySandboxRunner implements SandboxRunner {
  readonly jobs: InMemorySandboxJob[] = [];

  constructor(private readonly workerProtocolVersions: readonly number[] = [1]) {}

  async create(spec: JobSandboxSpec): Promise<InMemorySandboxJob> {
    const version = negotiateProtocolVersion(this.workerProtocolVersions);
    const job = new InMemorySandboxJob(spec, version);
    this.jobs.push(job);
    return job;
  }
}

export class InMemorySandboxJob implements SandboxJob {
  readonly commands: HostToWorkerMessage[] = [];
  private active?: ActiveRun;
  private runSequence = 0;
  private cancelled = false;
  private disposed = false;

  constructor(
    private readonly spec: JobSandboxSpec,
    private readonly protocolVersion: SandboxProtocolVersion,
  ) {}

  get id(): string { return this.spec.id; }
  get isRunning(): boolean { return Boolean(this.active && !this.active.terminal); }

  start(prompt: string): Promise<string> {
    if (this.disposed) return Promise.reject(new Error("Sandbox job is disposed"));
    if (this.cancelled) return Promise.reject(new Error("Sandbox job is cancelled"));
    if (this.isRunning) return Promise.reject(new Error("Sandbox job is already running"));

    const runId = `${this.id}:${++this.runSequence}`;
    const promise = new Promise<string>((resolve, reject) => {
      this.active = { id: runId, terminal: false, resolve, reject };
    });
    this.commands.push({
      protocolVersion: this.protocolVersion,
      type: "start",
      jobId: this.id,
      runId,
      prompt,
      deadlineAt: this.spec.deadlineAt.toISOString(),
      outputLimitBytes: this.spec.outputLimitBytes,
    });

    const remaining = this.spec.deadlineAt.getTime() - Date.now();
    if (remaining <= 0) this.timeout(runId);
    else this.active!.timer = setTimeout(() => this.timeout(runId), remaining);
    return promise;
  }

  async steer(message: string): Promise<void> {
    const run = this.requireActiveRun();
    this.commands.push({
      protocolVersion: this.protocolVersion,
      type: "steer",
      jobId: this.id,
      runId: run.id,
      message,
    });
  }

  async cancel(): Promise<void> {
    if (this.cancelled || this.disposed) return;
    this.cancelled = true;
    this.terminateLocally("operator", new Error("Sandbox job cancelled"));
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    if (!this.cancelled) this.terminateLocally("dispose", new Error("Sandbox job disposed"));
    this.disposed = true;
  }

  /** Returns false for stale, duplicate-terminal, or post-disposal events. */
  async receive(message: WorkerToHostMessage): Promise<boolean> {
    if (this.disposed) return false;
    if (message.protocolVersion !== this.protocolVersion) throw new Error("Worker used an unnegotiated protocol version");
    if (message.jobId !== this.id) throw new Error("Worker event has the wrong job identity");
    const run = this.active;
    if (!run || message.runId !== run.id || run.terminal) return false;

    if (message.type === "status") {
      await this.spec.events.status(message.message);
      return true;
    }
    if (message.type === "approval_request") {
      const answer = this.spec.events.approval
        ? await this.spec.events.approval({
            kind: message.kind,
            title: message.title,
            detail: message.detail,
            ...(message.choices ? { choices: message.choices } : {}),
          })
        : "no";
      this.commands.push({
        protocolVersion: this.protocolVersion,
        type: "approval_response",
        jobId: this.id,
        runId: run.id,
        requestId: message.requestId,
        answer,
      });
      return true;
    }
    if (message.type === "completion") {
      if (Buffer.byteLength(message.output, "utf8") > this.spec.outputLimitBytes) {
        this.finish(run, new SandboxFailure("Worker output exceeded its byte limit", "output_limit_exceeded", false));
      } else {
        this.finish(run, message.output);
      }
      return true;
    }

    this.finish(run, new SandboxFailure(message.message, message.code, message.retryable));
    return true;
  }

  private requireActiveRun(): ActiveRun {
    if (this.disposed) throw new Error("Sandbox job is disposed");
    if (this.cancelled) throw new Error("Sandbox job is cancelled");
    if (!this.active || this.active.terminal) throw new Error("Sandbox job is not running");
    return this.active;
  }

  private timeout(runId: string): void {
    const run = this.active;
    if (!run || run.id !== runId || run.terminal || this.disposed) return;
    this.pushCancel(run, "deadline");
    this.finish(run, new SandboxFailure("Sandbox job deadline exceeded", "deadline_exceeded", false));
  }

  private terminateLocally(reason: CancelMessage["reason"], error: Error): void {
    const run = this.active;
    if (!run || run.terminal) return;
    this.pushCancel(run, reason);
    this.finish(run, error);
  }

  private pushCancel(run: ActiveRun, reason: CancelMessage["reason"]): void {
    this.commands.push({
      protocolVersion: this.protocolVersion,
      type: "cancel",
      jobId: this.id,
      runId: run.id,
      reason,
    });
  }

  private finish(run: ActiveRun, result: string | Error): void {
    if (run.terminal) return;
    run.terminal = true;
    if (run.timer) clearTimeout(run.timer);
    if (result instanceof Error) run.reject(result);
    else run.resolve(result);
  }
}
