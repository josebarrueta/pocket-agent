import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import {
  createAgentSession,
  DefaultResourceLoader,
  getAgentDir,
  ModelRuntime,
  SessionManager,
  SettingsManager,
  type InlineExtension,
} from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import type { AppConfig, ToolDecision } from "./config.js";
import { createMcpExtension } from "./mcp.js";
import { SandboxFailure, type JobSandboxSpec, type SandboxJob, type SandboxRunner } from "./sandbox.js";
import type { ApprovalPort } from "./types.js";

function decisionFor(toolName: string, permissions: AppConfig["agent"]["permissions"]): ToolDecision {
  if (["read", "grep", "find", "ls"].includes(toolName)) return permissions.read;
  if (["edit", "write"].includes(toolName)) return permissions.write;
  if (toolName === "bash") return permissions.bash;
  return "allow";
}

/** Transitional in-process adapter; a container adapter will replace it. */
export class PiSandboxRunner implements SandboxRunner {
  constructor(
    private readonly config: AppConfig,
    private readonly approvals: ApprovalPort,
  ) {}

  async create(options: JobSandboxSpec): Promise<SandboxJob> {
    const sessionDir = join(this.config.stateDir, "pi-sessions");
    await mkdir(sessionDir, { recursive: true });

    const mcp = await createMcpExtension({
      servers: this.config.mcpServers,
      approvals: this.approvals,
      conversationId: options.conversationId,
      scopeId: options.id,
    });

    const safetyExtension: InlineExtension = (pi) => {
      pi.registerTool({
        name: "ask_operator",
        label: "Ask operator",
        description: "Ask the operator a blocking question when their input is required",
        parameters: Type.Object({
          question: Type.String(),
          choices: Type.Optional(Type.Array(Type.String(), { maxItems: 8 })),
        }),
        execute: async (_id, params) => {
          const answer = await this.approvals.request(options.conversationId, {
            kind: "question",
            scopeId: options.id,
            title: "Agent needs input",
            detail: params.question,
            ...(params.choices?.length ? { choices: params.choices } : {}),
          });
          return { content: [{ type: "text", text: answer }], details: {} };
        },
      });

      pi.on("tool_call", async (event) => {
        const decision = decisionFor(event.toolName, this.config.agent.permissions);
        if (decision === "allow") return;
        if (decision === "deny") return { block: true, reason: `${event.toolName} is denied by policy` };
        const answer = await this.approvals.request(options.conversationId, {
          kind: "agent-tool",
          scopeId: options.id,
          title: `Allow Pi tool ${event.toolName}?`,
          detail: JSON.stringify(event.input, null, 2).slice(0, 8_000),
          choices: ["yes", "no"],
        });
        if (answer !== "yes") return { block: true, reason: "Denied by operator" };
      });
    };

    const agentDir = getAgentDir();
    const settingsManager = SettingsManager.create(options.workspacePath, agentDir, { projectTrusted: false });
    const resourceLoader = new DefaultResourceLoader({
      cwd: options.workspacePath,
      agentDir,
      settingsManager,
      noExtensions: true,
      noSkills: true,
      noPromptTemplates: true,
      noThemes: true,
      extensionFactories: [safetyExtension, mcp.extension],
    });
    await resourceLoader.reload();

    const modelRuntime = await ModelRuntime.create();
    let model;
    if (this.config.agent.model) {
      const separator = this.config.agent.model.indexOf("/");
      if (separator < 1) throw new Error("agent.model must be provider/model-id");
      const provider = this.config.agent.model.slice(0, separator);
      const modelId = this.config.agent.model.slice(separator + 1);
      model = modelRuntime.getModel(provider, modelId);
      if (!model) throw new Error(`Unknown model ${this.config.agent.model}`);
    }

    const created = await createAgentSession({
      cwd: options.workspacePath,
      modelRuntime,
      resourceLoader,
      settingsManager,
      sessionManager: SessionManager.create(options.workspacePath, sessionDir),
      thinkingLevel: this.config.agent.thinking,
      ...(model ? { model } : {}),
    });

    return new PiSandboxJob(options, created.session, async () => {
      created.session.dispose();
      await mcp.close();
    });
  }
}

class PiSandboxJob implements SandboxJob {
  private running = false;
  private cancelled = false;
  private disposed = false;
  private lastToolStatusAt = 0;

  constructor(
    private readonly spec: JobSandboxSpec,
    private readonly session: Awaited<ReturnType<typeof createAgentSession>>["session"],
    private readonly cleanup: () => Promise<void>,
  ) {
    session.subscribe((event) => {
      if (event.type === "tool_execution_start" && Date.now() - this.lastToolStatusAt > 5_000) {
        this.lastToolStatusAt = Date.now();
        if (!this.disposed) void this.spec.events.status(`🔧 ${event.toolName}`).catch(console.error);
      }
    });
  }

  get id(): string { return this.spec.id; }

  get isRunning(): boolean {
    return this.running;
  }

  async start(prompt: string): Promise<string> {
    if (this.disposed) throw new Error("Sandbox job is disposed");
    if (this.cancelled) throw new Error("Sandbox job is cancelled");
    if (this.running) throw new Error("Sandbox job is already running");
    const remaining = this.spec.deadlineAt.getTime() - Date.now();
    if (remaining <= 0) throw new SandboxFailure("Sandbox job deadline exceeded", "deadline_exceeded", false);

    this.running = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([
        this.session.prompt(prompt),
        new Promise<never>((_resolve, reject) => {
          timer = setTimeout(() => {
            void this.session.abort();
            reject(new SandboxFailure("Sandbox job deadline exceeded", "deadline_exceeded", false));
          }, remaining);
        }),
      ]);
      const output = this.session.getLastAssistantText() || "(Agent completed without a text response.)";
      if (Buffer.byteLength(output, "utf8") > this.spec.outputLimitBytes) {
        throw new SandboxFailure("Worker output exceeded its byte limit", "output_limit_exceeded", false);
      }
      return output;
    } finally {
      if (timer) clearTimeout(timer);
      this.running = false;
    }
  }

  async steer(message: string): Promise<void> {
    if (!this.running) {
      await this.start(message);
      return;
    }
    await this.session.steer(message);
  }

  async cancel(): Promise<void> {
    if (this.cancelled || this.disposed) return;
    this.cancelled = true;
    await this.session.abort();
    this.running = false;
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    this.cancelled = true;
    try {
      if (this.running) await this.session.abort();
    } finally {
      this.running = false;
      await this.cleanup();
    }
  }
}
