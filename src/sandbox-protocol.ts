export const SANDBOX_PROTOCOL_VERSIONS = [1] as const;
export type SandboxProtocolVersion = (typeof SANDBOX_PROTOCOL_VERSIONS)[number];

export interface HostHello {
  type: "hello";
  supportedVersions: readonly SandboxProtocolVersion[];
}

export interface WorkerHello {
  type: "hello";
  supportedVersions: readonly number[];
}

interface HostMessageBase {
  protocolVersion: SandboxProtocolVersion;
  jobId: string;
  runId: string;
}

export interface StartMessage extends HostMessageBase {
  type: "start";
  prompt: string;
  deadlineAt: string;
  outputLimitBytes: number;
}

export interface SteerMessage extends HostMessageBase {
  type: "steer";
  message: string;
}

export interface CancelMessage extends HostMessageBase {
  type: "cancel";
  reason: "operator" | "deadline" | "dispose";
}

export type HostToWorkerMessage = StartMessage | SteerMessage | CancelMessage;

interface WorkerMessageBase {
  protocolVersion: SandboxProtocolVersion;
  jobId: string;
  runId: string;
}

export interface StatusMessage extends WorkerMessageBase {
  type: "status";
  message: string;
}

export interface CompletionMessage extends WorkerMessageBase {
  type: "completion";
  output: string;
}

export interface FailureMessage extends WorkerMessageBase {
  type: "failure";
  code: "worker_crash" | "deadline_exceeded" | "output_limit_exceeded" | "internal_error";
  message: string;
  retryable: boolean;
}

export type WorkerToHostMessage = StatusMessage | CompletionMessage | FailureMessage;

/** Selects the highest mutually supported protocol version. */
export function negotiateProtocolVersion(workerVersions: readonly number[]): SandboxProtocolVersion {
  for (const version of [...SANDBOX_PROTOCOL_VERSIONS].reverse()) {
    if (workerVersions.includes(version)) return version;
  }
  throw new Error(
    `No compatible sandbox protocol version (host: ${SANDBOX_PROTOCOL_VERSIONS.join(", ")}; worker: ${workerVersions.join(", ") || "none"})`,
  );
}
