import { randomUUID } from "node:crypto";
import type { SandboxJob, SandboxRunner } from "./sandbox.js";
import type { ApprovalPort, IncomingMessage, Messenger } from "./types.js";

const DEFAULT_JOB_TIMEOUT_MS = 60 * 60 * 1_000;
const DEFAULT_OUTPUT_LIMIT_BYTES = 1_000_000;

interface ControllerOptions {
  jobTimeoutMs?: number;
  outputLimitBytes?: number;
}

interface Job {
  id: string;
  conversationId: string;
  repo: string;
  run: SandboxJob;
  state: "idle" | "running" | "cancelled" | "failed";
  lastError?: string;
}

export class Controller {
  private readonly jobs = new Map<string, Job>();
  private readonly activeByConversation = new Map<string, string>();

  constructor(
    private readonly messenger: Messenger,
    private readonly approvals: ApprovalPort,
    private readonly sandboxes: SandboxRunner,
    private readonly repositories: Record<string, string>,
    private readonly options: ControllerOptions = {},
  ) {}

  async handle(message: IncomingMessage): Promise<void> {
    try {
      const [command = "", ...rest] = message.text.trim().split(/\s+/);
      switch (command.toLowerCase()) {
        case "/help":
        case "/start":
          await this.help(message.conversationId);
          return;
        case "/new":
          await this.newJob(message.conversationId, rest, false);
          return;
        case "/bug":
          await this.newJob(message.conversationId, rest, true);
          return;
        case "/answer": {
          const [id, ...answerParts] = rest;
          const accepted = Boolean(id) && this.approvals.answer(message.conversationId, id!, answerParts.join(" "));
          await this.messenger.send(message.conversationId, accepted ? `✅ Answered [${id}]` : "Unknown request or invalid choice.");
          return;
        }
        case "/cancel":
          await this.cancel(message.conversationId, rest[0]);
          return;
        case "/jobs":
          await this.listJobs(message.conversationId);
          return;
        case "/use":
          await this.useJob(message.conversationId, rest[0]);
          return;
        case "/status":
          await this.status(message.conversationId);
          return;
        case "/steer":
          await this.continueJob(message.conversationId, rest.join(" "));
          return;
        default:
          if (command.startsWith("/")) {
            await this.messenger.send(message.conversationId, "Unknown command. Send /help.");
            return;
          }
          await this.continueJob(message.conversationId, message.text);
      }
    } catch (error) {
      await this.messenger.send(message.conversationId, `❌ ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async close(): Promise<void> {
    await Promise.allSettled([...this.jobs.values()].map(async (job) => {
      await job.run.cancel();
      await job.run.dispose();
    }));
  }

  private async help(conversationId: string): Promise<void> {
    await this.messenger.send(conversationId, [
      "Pocket Agent commands",
      "/new <repo> <task> — start a Pi session",
      "/bug <repo> <description> — investigate and fix a bug",
      "/steer <message> — redirect active work",
      "/answer <id> <answer> — answer a question/approval",
      "/cancel [job-id] — stop work",
      "/jobs — list sessions",
      "/use <job-id> — select a session",
      "/status — active session status",
      `Repos: ${Object.keys(this.repositories).join(", ")}`,
    ].join("\n"));
  }

  private async newJob(conversationId: string, args: string[], bug: boolean): Promise<void> {
    const [repo, ...taskParts] = args;
    const task = taskParts.join(" ").trim();
    if (!repo || !task) throw new Error(`Usage: ${bug ? "/bug" : "/new"} <repo> <description>`);
    const cwd = this.repositories[repo];
    if (!cwd) throw new Error(`Unknown repo '${repo}'. Available: ${Object.keys(this.repositories).join(", ")}`);

    const id = randomUUID().slice(0, 8);
    const run = await this.sandboxes.create({
      id,
      workspacePath: cwd,
      conversationId,
      deadlineAt: new Date(Date.now() + (this.options.jobTimeoutMs ?? DEFAULT_JOB_TIMEOUT_MS)),
      outputLimitBytes: this.options.outputLimitBytes ?? DEFAULT_OUTPUT_LIMIT_BYTES,
      events: { status: (text) => this.messenger.send(conversationId, `[${id}] ${text}`) },
    });
    const job: Job = { id, conversationId, repo, run, state: "idle" };
    this.jobs.set(id, job);
    this.activeByConversation.set(conversationId, id);
    await this.messenger.send(conversationId, `🚀 [${id}] Starting in ${repo}.`);
    const prompt = bug
      ? `Investigate this bug, reproduce it if possible, implement a safe fix, run relevant tests, and summarize the result: ${task}`
      : task;
    this.runTurn(conversationId, job, prompt);
  }

  private async continueJob(conversationId: string, text: string): Promise<void> {
    if (!text.trim()) throw new Error("Message cannot be empty");
    const job = this.activeJob(conversationId);
    if (job.state === "cancelled") throw new Error("Active job was cancelled; create a new one with /new");
    if (job.run.isRunning) {
      await job.run.steer(text);
      await this.messenger.send(conversationId, `↪️ [${job.id}] Steering message queued.`);
      return;
    }
    this.runTurn(conversationId, job, text);
  }

  private runTurn(conversationId: string, job: Job, prompt: string): void {
    job.state = "running";
    void job.run.start(prompt).then(async (answer) => {
      job.state = "idle";
      await this.sendLong(conversationId, `✅ [${job.id}]\n${answer}`);
    }).catch(async (error: unknown) => {
      if (job.state === "cancelled") return;
      job.state = "failed";
      job.lastError = error instanceof Error ? error.message : String(error);
      await this.messenger.send(conversationId, `❌ [${job.id}] ${job.lastError}`);
    });
  }

  private async cancel(conversationId: string, requestedId?: string): Promise<void> {
    const id = requestedId ?? this.activeByConversation.get(conversationId);
    const job = id ? this.jobs.get(id) : undefined;
    if (!job || job.conversationId !== conversationId) throw new Error("No matching job");
    this.approvals.cancelScope(conversationId, job.id);
    job.state = "cancelled";
    await job.run.cancel();
    await job.run.dispose();
    await this.messenger.send(conversationId, `🛑 [${job.id}] Cancelled.`);
  }

  private async useJob(conversationId: string, id?: string): Promise<void> {
    if (!id) throw new Error("Usage: /use <job-id> (see /jobs)");
    const job = this.jobs.get(id);
    if (!job || job.conversationId !== conversationId) throw new Error("No matching job");
    if (job.state === "cancelled") throw new Error("Cancelled jobs cannot be resumed");
    this.activeByConversation.set(conversationId, id);
    await this.messenger.send(conversationId, `Active job: [${id}] ${job.repo} (${job.state})`);
  }

  private async listJobs(conversationId: string): Promise<void> {
    const active = this.activeByConversation.get(conversationId);
    const lines = [...this.jobs.values()]
      .filter((job) => job.conversationId === conversationId)
      .map((job) => `${job.id === active ? "*" : " "} [${job.id}] ${job.repo} — ${job.state}`);
    await this.messenger.send(conversationId, lines.length ? lines.join("\n") : "No jobs. Use /new <repo> <task>.");
  }

  private async status(conversationId: string): Promise<void> {
    const job = this.activeJob(conversationId);
    await this.messenger.send(
      conversationId,
      `[${job.id}] ${job.repo} — ${job.state}${job.lastError ? `\n${job.lastError}` : ""}`,
    );
  }

  private activeJob(conversationId: string): Job {
    const id = this.activeByConversation.get(conversationId);
    const job = id ? this.jobs.get(id) : undefined;
    if (!job || job.conversationId !== conversationId) throw new Error("No active job. Use /new <repo> <task>.");
    return job;
  }

  private async sendLong(conversationId: string, text: string): Promise<void> {
    const chunks = text.match(/[\s\S]{1,3500}/g) ?? [text];
    for (const chunk of chunks) await this.messenger.send(conversationId, chunk);
  }
}
