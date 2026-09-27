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
import type { AgentFactory, AgentRun, ApprovalPort } from "./types.js";

function decisionFor(toolName: string, permissions: AppConfig["agent"]["permissions"]): ToolDecision {
  if (["read", "grep", "find", "ls"].includes(toolName)) return permissions.read;
  if (["edit", "write"].includes(toolName)) return permissions.write;
  if (toolName === "bash") return permissions.bash;
  return "allow";
}

export class PiAgentFactory implements AgentFactory {
  constructor(
    private readonly config: AppConfig,
    private readonly approvals: ApprovalPort,
  ) {}

  async create(options: Parameters<AgentFactory["create"]>[0]): Promise<AgentRun> {
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
    const settingsManager = SettingsManager.create(options.cwd, agentDir, { projectTrusted: false });
    const resourceLoader = new DefaultResourceLoader({
      cwd: options.cwd,
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
      cwd: options.cwd,
      modelRuntime,
      resourceLoader,
      settingsManager,
      sessionManager: SessionManager.create(options.cwd, sessionDir),
      thinkingLevel: this.config.agent.thinking,
      ...(model ? { model } : {}),
    });

    return new PiAgentRun(options.id, created.session, options.events, async () => {
      created.session.dispose();
      await mcp.close();
    });
  }
}

class PiAgentRun implements AgentRun {
  private running = false;
  private disposed = false;
  private lastToolStatusAt = 0;

  constructor(
    readonly id: string,
    private readonly session: Awaited<ReturnType<typeof createAgentSession>>["session"],
    private readonly events: { status(message: string): Promise<void> },
    private readonly cleanup: () => Promise<void>,
  ) {
    session.subscribe((event) => {
      if (event.type === "tool_execution_start" && Date.now() - this.lastToolStatusAt > 5_000) {
        this.lastToolStatusAt = Date.now();
        void this.events.status(`🔧 ${event.toolName}`).catch(console.error);
      }
    });
  }

  get isRunning(): boolean {
    return this.running;
  }

  async start(prompt: string): Promise<string> {
    if (this.running) throw new Error("agent is already running");
    this.running = true;
    try {
      await this.session.prompt(prompt);
      return this.session.getLastAssistantText() || "(Agent completed without a text response.)";
    } finally {
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
    await this.session.abort();
    this.running = false;
  }

  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    await this.cleanup();
  }
}
