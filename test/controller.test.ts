import assert from "node:assert/strict";
import test from "node:test";
import { Controller } from "../src/controller.js";
import { InMemorySandboxRunner } from "../src/in-memory-sandbox.js";
import type { StartMessage } from "../src/sandbox-protocol.js";
import type { ApprovalPort, ApprovalRequest, IncomingMessage, Messenger } from "../src/types.js";
import type { JobWorkspace, WorkspacePatch, WorkspaceProvider } from "../src/workspace.js";

class FakeMessenger implements Messenger {
  sent: Array<{ conversationId: string; text: string }> = [];
  async start(): Promise<void> {}
  async send(conversationId: string, text: string): Promise<void> { this.sent.push({ conversationId, text }); }
  async close(): Promise<void> {}
}

class FakeApprovals implements ApprovalPort {
  answerResult = false;
  requests: Array<{ conversationId: string; request: ApprovalRequest }> = [];
  async request(conversationId: string, request: ApprovalRequest): Promise<string> {
    this.requests.push({ conversationId, request });
    return "yes";
  }
  answer(): boolean { return this.answerResult; }
  cancelScope(): void {}
}

class FakeWorkspace implements JobWorkspace {
  disposed = false;
  patch: WorkspacePatch = { patch: "", files: [] };
  constructor(readonly jobId: string, readonly path: string) {}
  async exportPatch(): Promise<WorkspacePatch> { return this.patch; }
  async dispose(): Promise<void> { this.disposed = true; }
}

class FakeWorkspaces implements WorkspaceProvider {
  readonly aliases = ["app"];
  readonly created: FakeWorkspace[] = [];
  async create(jobId: string, alias: string): Promise<FakeWorkspace> {
    if (alias !== "app") throw new Error(`Unknown repository alias '${alias}'`);
    const workspace = new FakeWorkspace(jobId, `/disposable/${jobId}`);
    this.created.push(workspace);
    return workspace;
  }
}

function message(text: string, conversationId = "operator"): IncomingMessage {
  return { id: "1", conversationId, senderId: conversationId, text, receivedAt: new Date() };
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function firstStart(runner: InMemorySandboxRunner): StartMessage {
  const command = runner.jobs[0]?.commands.find((item) => item.type === "start");
  assert.ok(command && command.type === "start");
  return command;
}

test("starts a job only in an operator-configured repository", async () => {
  const messenger = new FakeMessenger();
  const runner = new InMemorySandboxRunner();
  const workspaces = new FakeWorkspaces();
  const controller = new Controller(messenger, new FakeApprovals(), runner, workspaces);

  await controller.handle(message("/new app fix the login race"));
  const start = firstStart(runner);
  assert.equal(start.prompt, "fix the login race");
  assert.equal(runner.jobs[0]?.id, start.jobId);
  assert.equal(workspaces.created[0]?.path, `/disposable/${start.jobId}`);
  workspaces.created[0]!.patch = { patch: "diff", files: [{ path: "src/app.ts", status: "modified" }] };

  await runner.jobs[0]!.receive({
    protocolVersion: 1,
    type: "completion",
    jobId: start.jobId,
    runId: start.runId,
    output: "done",
  });
  await tick();
  assert.match(messenger.sent.at(-1)?.text ?? "", /done/);
  assert.match(messenger.sent.at(-1)?.text ?? "", /modified "src\/app.ts"/);

  await controller.handle(message("/new unknown do something"));
  assert.match(messenger.sent.at(-1)?.text ?? "", /Unknown repo/);
});

test("routes worker approvals through the conversation-scoped approval broker", async () => {
  const approvals = new FakeApprovals();
  const runner = new InMemorySandboxRunner();
  const controller = new Controller(new FakeMessenger(), approvals, runner, new FakeWorkspaces());

  await controller.handle(message("/new app task"));
  const start = firstStart(runner);
  await runner.jobs[0]!.receive({
    protocolVersion: 1,
    type: "approval_request",
    jobId: start.jobId,
    runId: start.runId,
    requestId: "worker-request",
    kind: "agent-tool",
    title: "Allow bash?",
    detail: "npm test",
    choices: ["yes", "no"],
  });

  assert.deepEqual(approvals.requests, [{
    conversationId: "operator",
    request: {
      kind: "agent-tool",
      scopeId: start.jobId,
      title: "Allow bash?",
      detail: "npm test",
      choices: ["yes", "no"],
    },
  }]);
  assert.equal(runner.jobs[0]?.commands.at(-1)?.type, "approval_response");
  await controller.close();
});

test("bug command adds a reproduce, fix, and test instruction", async () => {
  const runner = new InMemorySandboxRunner();
  const controller = new Controller(new FakeMessenger(), new FakeApprovals(), runner, new FakeWorkspaces());

  await controller.handle(message("/bug app crashes on empty input"));

  assert.match(firstStart(runner).prompt, /reproduce it/i);
  assert.match(firstStart(runner).prompt, /run relevant tests/i);
  await controller.close();
});

test("jobs cannot be selected from a different conversation", async () => {
  const messenger = new FakeMessenger();
  const runner = new InMemorySandboxRunner();
  const controller = new Controller(messenger, new FakeApprovals(), runner, new FakeWorkspaces());

  await controller.handle(message("/new app task", "alice"));
  const id = runner.jobs[0]!.id;
  await controller.handle(message(`/use ${id}`, "bob"));

  assert.equal(messenger.sent.at(-1)?.conversationId, "bob");
  assert.match(messenger.sent.at(-1)?.text ?? "", /No matching job/);
  await controller.close();
});

test("steers and cancels a running sandbox job", async () => {
  const messenger = new FakeMessenger();
  const approvals = new FakeApprovals();
  const runner = new InMemorySandboxRunner();
  const workspaces = new FakeWorkspaces();
  const controller = new Controller(messenger, approvals, runner, workspaces);

  await controller.handle(message("/new app initial task"));
  await controller.handle(message("/steer focus on the parser"));
  assert.equal(runner.jobs[0]?.commands.at(-1)?.type, "steer");

  await controller.handle(message("/cancel"));
  assert.equal(runner.jobs[0]?.commands.at(-1)?.type, "cancel");
  assert.equal(workspaces.created[0]?.disposed, true);
  assert.match(messenger.sent.at(-1)?.text ?? "", /Cancelled/);
});

test("reports a worker crash as a failed job", async () => {
  const messenger = new FakeMessenger();
  const runner = new InMemorySandboxRunner();
  const workspaces = new FakeWorkspaces();
  const controller = new Controller(messenger, new FakeApprovals(), runner, workspaces);

  await controller.handle(message("/new app task"));
  const start = firstStart(runner);
  await runner.jobs[0]!.receive({
    protocolVersion: 1,
    type: "failure",
    jobId: start.jobId,
    runId: start.runId,
    code: "worker_crash",
    message: "worker exited with code 137",
    retryable: true,
  });
  await tick();

  assert.match(messenger.sent.at(-1)?.text ?? "", /worker exited with code 137/);
  assert.equal(workspaces.created[0]?.disposed, true);
});
