import { execFile, spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { randomUUID } from "node:crypto";
import { lstat, mkdtemp, rename, rm } from "node:fs/promises";
import { basename, dirname, isAbsolute, join } from "node:path";
import { promisify } from "node:util";
import {
  SANDBOX_PROTOCOL_VERSIONS,
  negotiateProtocolVersion,
  type FailureMessage,
  type SandboxProtocolVersion,
  type WorkerToHostMessage,
} from "./sandbox-protocol.js";
import { SandboxFailure, type JobSandboxSpec, type SandboxJob, type SandboxRunner } from "./sandbox.js";

const execFileAsync = promisify(execFile);
const MANAGED_LABEL = "pocket-agent.managed=true";
const MAX_PROTOCOL_BUFFER = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES = 64 * 1024;

export interface DockerSandboxLimits {
  cpus: number;
  memoryBytes: number;
  pids: number;
  temporaryStorageBytes: number;
  workspaceStorageBytes: number;
  openFiles: number;
}

export interface DockerSandboxRunnerOptions {
  dockerPath: string;
  tarPath?: string;
  image: string;
  protocolHandshakeMs?: number;
  commandTimeoutMs?: number;
  limits?: Partial<DockerSandboxLimits>;
  /** Test-only escape hatch for locally built fixture tags. */
  allowUnpinnedImageForTests?: boolean;
}

const DEFAULT_LIMITS: DockerSandboxLimits = {
  cpus: 1,
  memoryBytes: 1024 * 1024 * 1024,
  pids: 256,
  temporaryStorageBytes: 256 * 1024 * 1024,
  workspaceStorageBytes: 768 * 1024 * 1024,
  openFiles: 1024,
};

export class DockerSandboxRunner implements SandboxRunner {
  private readonly dockerPath: string;
  private readonly tarPath: string;
  private readonly image: string;
  private readonly handshakeMs: number;
  private readonly commandTimeoutMs: number;
  private readonly limits: DockerSandboxLimits;

  constructor(options: DockerSandboxRunnerOptions) {
    if (!isAbsolute(options.dockerPath)) throw new Error("dockerPath must be absolute");
    this.tarPath = options.tarPath ?? "/usr/bin/tar";
    if (!isAbsolute(this.tarPath)) throw new Error("tarPath must be absolute");
    if (!options.image.trim()) throw new Error("Worker image is required");
    if (!options.allowUnpinnedImageForTests && !/@sha256:[a-fA-F0-9]{64}$/.test(options.image)) {
      throw new Error("Worker image must be pinned by a complete sha256 digest");
    }
    this.dockerPath = options.dockerPath;
    this.image = options.image;
    this.handshakeMs = options.protocolHandshakeMs ?? 5_000;
    this.commandTimeoutMs = options.commandTimeoutMs ?? 30_000;
    this.limits = { ...DEFAULT_LIMITS, ...options.limits };
    assertLimits(this.limits);
  }

  async create(spec: JobSandboxSpec): Promise<SandboxJob> {
    validateJobId(spec.id);
    const workspace = await lstat(spec.workspacePath);
    if (!workspace.isDirectory() || workspace.isSymbolicLink()) {
      throw new Error("Disposable workspace must be a real directory");
    }

    const suffix = randomUUID().slice(0, 8);
    const containerName = `pocket-agent-job-${spec.id}-${suffix}`;
    const volumeName = `pocket-agent-workspace-${spec.id}-${suffix}`;
    await this.docker([
      "volume", "create",
      "--label", MANAGED_LABEL,
      "--label", `pocket-agent.job-id=${spec.id}`,
      "--driver", "local",
      "--opt", "type=tmpfs",
      "--opt", "device=tmpfs",
      "--opt", `o=size=${this.limits.workspaceStorageBytes},uid=65532,gid=65532,mode=0700`,
      volumeName,
    ]);

    try {
      await this.importWorkspace(spec.workspacePath, volumeName, `${containerName}-import`);
      await this.docker([
        "create", "--interactive",
        "--name", containerName,
        "--hostname", "pocket-agent-worker",
        "--label", MANAGED_LABEL,
        "--label", `pocket-agent.job-id=${spec.id}`,
        "--user", "65532:65532",
        "--read-only",
        "--cap-drop", "ALL",
        "--security-opt", "no-new-privileges=true",
        "--network", "none",
        "--ipc", "none",
        "--pids-limit", String(this.limits.pids),
        "--memory", String(this.limits.memoryBytes),
        "--cpus", String(this.limits.cpus),
        "--ulimit", `nofile=${this.limits.openFiles}:${this.limits.openFiles}`,
        "--ulimit", "core=0:0",
        "--tmpfs", `/tmp:rw,nosuid,nodev,noexec,size=${this.limits.temporaryStorageBytes},mode=1777`,
        "--mount", `type=volume,src=${volumeName},dst=/workspace`,
        "--stop-timeout", "1",
        "--log-driver", "none",
        this.image,
      ]);
    } catch (error) {
      await this.dockerIgnoringFailure(["rm", "--force", containerName]);
      await this.dockerIgnoringFailure(["volume", "rm", "--force", volumeName]);
      throw error;
    }

    return new DockerSandboxJob(
      spec,
      this.dockerPath,
      this.tarPath,
      this.image,
      containerName,
      volumeName,
      this.handshakeMs,
      this.commandTimeoutMs,
    );
  }

  /** Removes containers and volumes left by a previous single-controller instance. */
  async reconcile(): Promise<void> {
    const containers = splitLines(await this.docker([
      "ps", "--all", "--quiet", "--filter", `label=${MANAGED_LABEL}`,
    ]));
    if (containers.length) await this.docker(["rm", "--force", ...containers]);
    const volumes = splitLines(await this.docker([
      "volume", "ls", "--quiet", "--filter", `label=${MANAGED_LABEL}`,
    ]));
    if (volumes.length) await this.docker(["volume", "rm", "--force", ...volumes]);
  }

  private async importWorkspace(workspacePath: string, volumeName: string, importerName: string): Promise<void> {
    const archive = spawn(this.tarPath, ["-C", workspacePath, "-c", "-f", "-", "."], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    const importer = spawn(this.dockerPath, [
      "run", "--rm", "--interactive",
      "--name", importerName,
      "--user", "65532:65532",
      "--read-only",
      "--cap-drop", "ALL",
      "--security-opt", "no-new-privileges=true",
      "--network", "none",
      "--pids-limit", "64",
      "--memory", "134217728",
      "--cpus", "0.5",
      "--mount", `type=volume,src=${volumeName},dst=/workspace`,
      "--entrypoint", "tar",
      this.image,
      "-x", "-f", "-", "-C", "/workspace",
    ], { stdio: ["pipe", "pipe", "pipe"], env: process.env });
    archive.stdout.pipe(importer.stdin);

    const [archiveResult, importerResult] = await Promise.all([
      waitForProcess(archive, MAX_DIAGNOSTIC_BYTES),
      waitForProcess(importer, MAX_DIAGNOSTIC_BYTES),
    ]);
    if (archiveResult.code !== 0) throw new Error(`Workspace archive failed: ${archiveResult.stderr || `exit ${archiveResult.code}`}`);
    if (importerResult.code !== 0) throw new Error(`Workspace import failed: ${importerResult.stderr || `exit ${importerResult.code}`}`);
  }

  private async docker(args: readonly string[]): Promise<string> {
    const { stdout } = await execFileAsync(this.dockerPath, args, {
      encoding: "utf8",
      timeout: this.commandTimeoutMs,
      maxBuffer: 4 * 1024 * 1024,
      env: process.env,
    });
    return stdout.trim();
  }

  private async dockerIgnoringFailure(args: readonly string[]): Promise<void> {
    try { await this.docker(args); } catch { /* best-effort rollback */ }
  }
}

interface ActiveRun {
  id: string;
  terminal: boolean;
  settling: boolean;
  resolve(output: string): void;
  reject(error: Error): void;
  timer?: ReturnType<typeof setTimeout>;
}

class DockerSandboxJob implements SandboxJob {
  private child?: ChildProcessWithoutNullStreams;
  private protocolVersion?: SandboxProtocolVersion;
  private handshake?: { promise: Promise<void>; resolve(): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> };
  private active?: ActiveRun;
  private runSequence = 0;
  private stdoutBuffer = "";
  private diagnostics = "";
  private protocolOutputBytes = 0;
  private cancelled = false;
  private disposed = false;
  private broken = false;
  private cleanup?: Promise<void>;

  constructor(
    private readonly spec: JobSandboxSpec,
    private readonly dockerPath: string,
    private readonly hostTarPath: string,
    private readonly image: string,
    readonly containerName: string,
    readonly volumeName: string,
    private readonly handshakeMs: number,
    private readonly commandTimeoutMs: number,
  ) {}

  get id(): string { return this.spec.id; }
  get isRunning(): boolean { return Boolean(this.active && (!this.active.terminal || this.active.settling)); }

  async start(prompt: string): Promise<string> {
    if (this.disposed) throw new Error("Sandbox job is disposed");
    if (this.cancelled) throw new Error("Sandbox job is cancelled");
    if (this.broken) throw new Error("Sandbox worker is unavailable");
    if (this.isRunning) throw new Error("Sandbox job is already running");
    if (!prompt || Buffer.byteLength(prompt, "utf8") > 1024 * 1024) throw new Error("Prompt is empty or too large");
    if (Date.now() >= this.spec.deadlineAt.getTime()) {
      throw new SandboxFailure("Sandbox job deadline exceeded", "deadline_exceeded", false);
    }

    await this.ensureConnected();
    this.protocolOutputBytes = 0;
    const runId = `${this.id}:${++this.runSequence}`;
    const result = new Promise<string>((resolve, reject) => {
      this.active = { id: runId, terminal: false, settling: false, resolve, reject };
    });
    const remaining = this.spec.deadlineAt.getTime() - Date.now();
    this.active!.timer = setTimeout(() => this.timeout(runId), remaining);
    try {
      await this.send({
        protocolVersion: this.protocolVersion!,
        type: "start",
        jobId: this.id,
        runId,
        prompt,
        deadlineAt: this.spec.deadlineAt.toISOString(),
        outputLimitBytes: this.spec.outputLimitBytes,
      });
    } catch (error) {
      this.finish(this.active!, error instanceof Error ? error : new Error(String(error)));
    }
    return result;
  }

  async steer(message: string): Promise<void> {
    const run = this.requireActive();
    await this.send({
      protocolVersion: this.protocolVersion!,
      type: "steer",
      jobId: this.id,
      runId: run.id,
      message,
    });
  }

  async cancel(): Promise<void> {
    if (this.cancelled || this.disposed) return;
    this.cancelled = true;
    const run = this.active;
    if (run && !run.terminal && this.child && this.protocolVersion) {
      try {
        await this.send({
          protocolVersion: this.protocolVersion,
          type: "cancel",
          jobId: this.id,
          runId: run.id,
          reason: "operator",
        });
      } catch { /* force removal below */ }
      this.finish(run, new Error("Sandbox job cancelled"));
    }
    await this.disposeResources();
  }

  async dispose(): Promise<void> {
    if (this.disposed) return this.cleanup;
    this.disposed = true;
    const run = this.active;
    if (run && !run.terminal) this.finish(run, new Error("Sandbox job disposed"));
    await this.disposeResources();
  }

  private async ensureConnected(): Promise<void> {
    if (this.protocolVersion) return;
    if (this.handshake) return this.handshake.promise;

    let resolveHandshake!: () => void;
    let rejectHandshake!: (error: Error) => void;
    const promise = new Promise<void>((resolve, reject) => {
      resolveHandshake = resolve;
      rejectHandshake = reject;
    });
    const timer = setTimeout(() => {
      const error = new Error("Worker protocol handshake timed out");
      rejectHandshake(error);
      void this.protocolViolation(error);
    }, this.handshakeMs);
    this.handshake = { promise, resolve: resolveHandshake, reject: rejectHandshake, timer };

    this.child = spawn(this.dockerPath, ["start", "--attach", "--interactive", this.containerName], {
      stdio: ["pipe", "pipe", "pipe"],
      env: process.env,
    });
    this.child.stdout.setEncoding("utf8");
    this.child.stderr.setEncoding("utf8");
    this.child.stdout.on("data", (chunk: string) => this.onStdout(chunk));
    this.child.stderr.on("data", (chunk: string) => {
      this.diagnostics = appendBounded(this.diagnostics, chunk, MAX_DIAGNOSTIC_BYTES);
    });
    this.child.once("error", (error) => this.onExit(null, error));
    this.child.once("close", (code) => this.onExit(code, undefined));
    return promise;
  }

  private onStdout(chunk: string): void {
    this.stdoutBuffer += chunk;
    const limit = Math.min(MAX_PROTOCOL_BUFFER, this.spec.outputLimitBytes + 64 * 1024);
    if (Buffer.byteLength(this.stdoutBuffer, "utf8") > limit && !this.stdoutBuffer.includes("\n")) {
      void this.protocolViolation(new Error("Worker protocol message exceeded its limit"));
      return;
    }
    let newline: number;
    while ((newline = this.stdoutBuffer.indexOf("\n")) >= 0) {
      const line = this.stdoutBuffer.slice(0, newline);
      this.stdoutBuffer = this.stdoutBuffer.slice(newline + 1);
      const lineBytes = Buffer.byteLength(line, "utf8") + 1;
      this.protocolOutputBytes += lineBytes;
      if (lineBytes > limit || this.protocolOutputBytes > this.spec.outputLimitBytes + 1024 * 1024) {
        void this.protocolViolation(new Error("Worker protocol message exceeded its limit"));
        return;
      }
      if (!line) {
        void this.protocolViolation(new Error("Worker sent an empty protocol message"));
        return;
      }
      this.receiveLine(line);
    }
  }

  private receiveLine(line: string): void {
    let value: unknown;
    try { value = JSON.parse(line); } catch {
      void this.protocolViolation(new Error("Worker sent malformed JSON"));
      return;
    }
    if (!isRecord(value)) {
      void this.protocolViolation(new Error("Worker message must be an object"));
      return;
    }

    if (!this.protocolVersion) {
      if (!hasOnlyKeys(value, ["type", "supportedVersions"]) || value.type !== "hello" || !Array.isArray(value.supportedVersions) || !value.supportedVersions.every(Number.isSafeInteger)) {
        void this.protocolViolation(new Error("Worker did not send a valid protocol hello"));
        return;
      }
      try {
        this.protocolVersion = negotiateProtocolVersion(value.supportedVersions);
        void this.send({ type: "hello", supportedVersions: SANDBOX_PROTOCOL_VERSIONS }).then(() => {
          if (!this.handshake) return;
          clearTimeout(this.handshake.timer);
          this.handshake.resolve();
        }).catch((error: unknown) => this.protocolViolation(error instanceof Error ? error : new Error(String(error))));
      } catch (error) {
        void this.protocolViolation(error instanceof Error ? error : new Error(String(error)));
      }
      return;
    }

    if (!isWorkerMessage(value, this.protocolVersion)) {
      void this.protocolViolation(new Error("Worker sent an invalid protocol message"));
      return;
    }
    const message = value as unknown as WorkerToHostMessage;
    if (message.jobId !== this.id) {
      void this.protocolViolation(new Error("Worker message has the wrong job identity"));
      return;
    }
    const run = this.active;
    if (!run || run.terminal || message.runId !== run.id) return;
    if (message.type === "status") {
      void this.spec.events.status(message.message).catch(() => {});
    } else if (message.type === "completion") {
      if (Buffer.byteLength(message.output, "utf8") > this.spec.outputLimitBytes) {
        this.finish(run, new SandboxFailure("Worker output exceeded its byte limit", "output_limit_exceeded", false));
      } else {
        this.finishWithWorkspaceExport(run, message.output);
      }
    } else {
      this.finish(run, new SandboxFailure(message.message, message.code, message.retryable));
    }
  }

  private onExit(code: number | null, spawnError?: Error): void {
    if (this.disposed || this.cancelled) return;
    this.broken = true;
    const detail = spawnError?.message || this.diagnostics.trim() || `worker exited with code ${code ?? "unknown"}`;
    const error = new SandboxFailure(detail, "worker_crash", true);
    if (this.handshake) {
      clearTimeout(this.handshake.timer);
      this.handshake.reject(error);
    }
    const run = this.active;
    if (run && !run.terminal) this.finish(run, error);
  }

  private async protocolViolation(error: Error): Promise<void> {
    if (this.broken || this.disposed) return;
    this.broken = true;
    if (this.handshake) {
      clearTimeout(this.handshake.timer);
      this.handshake.reject(error);
    }
    const run = this.active;
    if (run && !run.terminal) this.finish(run, new SandboxFailure(error.message, "internal_error", false));
    await this.disposeResources();
  }

  private timeout(runId: string): void {
    const run = this.active;
    if (!run || run.id !== runId || run.terminal) return;
    if (this.protocolVersion) {
      void this.send({
        protocolVersion: this.protocolVersion,
        type: "cancel",
        jobId: this.id,
        runId: run.id,
        reason: "deadline",
      }).catch(() => {});
    }
    this.finish(run, new SandboxFailure("Sandbox job deadline exceeded", "deadline_exceeded", false));
    void this.disposeResources();
  }

  private requireActive(): ActiveRun {
    if (this.disposed) throw new Error("Sandbox job is disposed");
    if (this.cancelled) throw new Error("Sandbox job is cancelled");
    if (!this.active || this.active.terminal) throw new Error("Sandbox job is not running");
    return this.active;
  }

  private finishWithWorkspaceExport(run: ActiveRun, output: string): void {
    if (run.terminal) return;
    run.terminal = true;
    run.settling = true;
    if (run.timer) clearTimeout(run.timer);
    void this.captureWorkspace().then(
      () => run.resolve(output),
      (error: unknown) => run.reject(error instanceof Error ? error : new Error(String(error))),
    ).finally(() => { run.settling = false; });
  }

  private async captureWorkspace(): Promise<void> {
    await execFileAsync(this.dockerPath, ["pause", this.containerName], {
      timeout: this.commandTimeoutMs,
      maxBuffer: 1024 * 1024,
      env: process.env,
    });
    const parent = dirname(this.spec.workspacePath);
    const staging = await mkdtemp(join(parent, ".docker-export-"));
    try {
      const exporter = spawn(this.dockerPath, [
        "run", "--rm",
        "--user", "65532:65532",
        "--read-only",
        "--cap-drop", "ALL",
        "--security-opt", "no-new-privileges=true",
        "--network", "none",
        "--pids-limit", "64",
        "--memory", "134217728",
        "--cpus", "0.5",
        "--mount", `type=volume,src=${this.volumeName},dst=/workspace,readonly`,
        "--entrypoint", "tar",
        this.image,
        "-c", "-f", "-", "-C", "/workspace", ".",
      ], { stdio: ["ignore", "pipe", "pipe"], env: process.env });
      const extractor = spawn(this.hostTarPath, [
        "--extract", "--file", "-", "--directory", staging,
        "--no-same-owner", "--no-same-permissions",
      ], { stdio: ["pipe", "pipe", "pipe"] });
      exporter.stdout.pipe(extractor.stdin);
      const [exportResult, extractResult] = await Promise.all([
        waitForProcess(exporter, MAX_DIAGNOSTIC_BYTES),
        waitForProcess(extractor, MAX_DIAGNOSTIC_BYTES),
      ]);
      if (exportResult.code !== 0) throw new Error(`Workspace export failed: ${exportResult.stderr || `exit ${exportResult.code}`}`);
      if (extractResult.code !== 0) throw new Error(`Workspace extraction failed: ${extractResult.stderr || `exit ${extractResult.code}`}`);

      const backup = join(parent, `.docker-export-old-${randomUUID()}`);
      await rename(this.spec.workspacePath, backup);
      try {
        await rename(staging, join(parent, basename(this.spec.workspacePath)));
      } catch (error) {
        await rename(backup, this.spec.workspacePath);
        throw error;
      }
      await rm(backup, { recursive: true, force: true });
    } finally {
      await rm(staging, { recursive: true, force: true });
      await dockerIgnoringFailure(this.dockerPath, ["unpause", this.containerName], this.commandTimeoutMs);
    }
  }

  private finish(run: ActiveRun, result: string | Error): void {
    if (run.terminal) return;
    run.terminal = true;
    run.settling = false;
    if (run.timer) clearTimeout(run.timer);
    if (result instanceof Error) run.reject(result);
    else run.resolve(result);
  }

  private async send(message: object): Promise<void> {
    const child = this.child;
    if (!child || child.stdin.destroyed || !child.stdin.writable) throw new Error("Worker control channel is closed");
    const payload = `${JSON.stringify(message)}\n`;
    if (child.stdin.write(payload)) return;
    await new Promise<void>((resolve, reject) => {
      child.stdin.once("drain", resolve);
      child.stdin.once("error", reject);
    });
  }

  private disposeResources(): Promise<void> {
    if (this.cleanup) return this.cleanup;
    this.cleanup = (async () => {
      await dockerIgnoringFailure(this.dockerPath, ["rm", "--force", this.containerName], this.commandTimeoutMs);
      await dockerIgnoringFailure(this.dockerPath, ["volume", "rm", "--force", this.volumeName], this.commandTimeoutMs);
      this.child?.stdin.destroy();
      this.child?.stdout.destroy();
      this.child?.stderr.destroy();
    })();
    return this.cleanup;
  }
}

function isWorkerMessage(value: Record<string, unknown>, version: SandboxProtocolVersion): boolean {
  if (value.protocolVersion !== version || typeof value.jobId !== "string" || typeof value.runId !== "string") return false;
  if (value.type === "status") {
    return hasOnlyKeys(value, ["protocolVersion", "type", "jobId", "runId", "message"]) && typeof value.message === "string";
  }
  if (value.type === "completion") {
    return hasOnlyKeys(value, ["protocolVersion", "type", "jobId", "runId", "output"]) && typeof value.output === "string";
  }
  if (value.type === "failure") {
    return hasOnlyKeys(value, ["protocolVersion", "type", "jobId", "runId", "code", "message", "retryable"]) &&
      typeof value.message === "string" && typeof value.retryable === "boolean" && isFailureCode(value.code);
  }
  return false;
}

function hasOnlyKeys(value: Record<string, unknown>, keys: readonly string[]): boolean {
  return Object.keys(value).every((key) => keys.includes(key));
}

function isFailureCode(code: unknown): code is FailureMessage["code"] {
  return ["worker_crash", "deadline_exceeded", "output_limit_exceeded", "internal_error"].includes(String(code));
}

async function dockerIgnoringFailure(dockerPath: string, args: readonly string[], timeout: number): Promise<void> {
  try {
    await execFileAsync(dockerPath, args, { timeout, maxBuffer: 1024 * 1024, env: process.env });
  } catch { /* resources may already be gone */ }
}

async function waitForProcess(
  child: ReturnType<typeof spawn>,
  maximumDiagnosticBytes: number,
): Promise<{ code: number | null; stderr: string }> {
  let stderr = "";
  child.stderr?.setEncoding("utf8");
  child.stderr?.on("data", (chunk: string) => {
    stderr = appendBounded(stderr, chunk, maximumDiagnosticBytes);
  });
  return new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code) => resolve({ code, stderr: stderr.trim() }));
  });
}

function appendBounded(current: string, addition: string, maximumBytes: number): string {
  const combined = current + addition;
  if (Buffer.byteLength(combined, "utf8") <= maximumBytes) return combined;
  return Buffer.from(combined, "utf8").subarray(-maximumBytes).toString("utf8");
}

function splitLines(output: string): string[] {
  return output.split(/\r?\n/).map((line) => line.trim()).filter(Boolean);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function validateJobId(id: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$/.test(id)) throw new Error("Invalid Docker sandbox job identity");
}

function assertLimits(limits: DockerSandboxLimits): void {
  for (const [name, value] of Object.entries(limits)) {
    if (!Number.isFinite(value) || value <= 0) throw new Error(`${name} must be positive`);
  }
  if (!Number.isSafeInteger(limits.memoryBytes) || !Number.isSafeInteger(limits.pids) ||
      !Number.isSafeInteger(limits.temporaryStorageBytes) || !Number.isSafeInteger(limits.openFiles)) {
    throw new Error("Docker byte, PID, and file limits must be integers");
  }
}
