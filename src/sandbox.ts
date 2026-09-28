import type { ConversationId } from "./types.js";

export interface SandboxEvents {
  status(message: string): Promise<void>;
}

export interface JobSandboxSpec {
  /** Unique, unguessable identity used by the worker and capability broker. */
  id: string;
  /** Trusted host path resolved from a configured repository alias. */
  workspacePath: string;
  conversationId: ConversationId;
  /** Absolute deadline for the lifetime of this job. */
  deadlineAt: Date;
  /** Maximum UTF-8 bytes accepted in a completion message. */
  outputLimitBytes: number;
  events: SandboxEvents;
}

export interface SandboxJob {
  readonly id: string;
  readonly isRunning: boolean;
  start(prompt: string): Promise<string>;
  steer(message: string): Promise<void>;
  cancel(): Promise<void>;
  dispose(): Promise<void>;
}

export interface SandboxRunner {
  create(spec: JobSandboxSpec): Promise<SandboxJob>;
}

export class SandboxFailure extends Error {
  constructor(
    message: string,
    readonly code: "worker_crash" | "deadline_exceeded" | "output_limit_exceeded" | "internal_error",
    readonly retryable: boolean,
  ) {
    super(message);
    this.name = "SandboxFailure";
  }
}
