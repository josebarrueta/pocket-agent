import assert from "node:assert/strict";
import test from "node:test";
import { Controller } from "../src/controller.js";
import { InMemorySandboxRunner } from "../src/in-memory-sandbox.js";
import type { StartMessage } from "../src/sandbox-protocol.js";
import type { ApprovalPort, IncomingMessage, Messenger } from "../src/types.js";

class FakeMessenger implements Messenger {
  sent: Array<{ conversationId: string; text: string }> = [];
  async start(): Promise<void> {}
  async send(conversationId: string, text: string): Promise<void> { this.sent.push({ conversationId, text }); }
  async close(): Promise<void> {}
}

class FakeApprovals implements ApprovalPort {
  answerResult = false;
  async request(): Promise<string> { return "yes"; }
  answer(): boolean { return this.answerResult; }
  cancelScope(): void {}
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
  const controller = new Controller(messenger, new FakeApprovals(), runner, { app: "/work/app" });

  await controller.handle(message("/new app fix the login race"));
  const start = firstStart(runner);
  assert.equal(start.prompt, "fix the login race");
  assert.equal(runner.jobs[0]?.id, start.jobId);

  await runner.jobs[0]!.receive({
    protocolVersion: 1,
    type: "completion",
    jobId: start.jobId,
    runId: start.runId,
    output: "done",
  });
  await tick();
  assert.match(messenger.sent.at(-1)?.text ?? "", /done/);

  await controller.handle(message("/new unknown do something"));
  assert.match(messenger.sent.at(-1)?.text ?? "", /Unknown repo/);
});

test("bug command adds a reproduce, fix, and test instruction", async () => {
  const runner = new InMemorySandboxRunner();
  const controller = new Controller(new FakeMessenger(), new FakeApprovals(), runner, { app: "/work/app" });

  await controller.handle(message("/bug app crashes on empty input"));

  assert.match(firstStart(runner).prompt, /reproduce it/i);
  assert.match(firstStart(runner).prompt, /run relevant tests/i);
  await controller.close();
});

test("jobs cannot be selected from a different conversation", async () => {
  const messenger = new FakeMessenger();
  const runner = new InMemorySandboxRunner();
  const controller = new Controller(messenger, new FakeApprovals(), runner, { app: "/work/app" });

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
  const controller = new Controller(messenger, approvals, runner, { app: "/work/app" });

  await controller.handle(message("/new app initial task"));
  await controller.handle(message("/steer focus on the parser"));
  assert.equal(runner.jobs[0]?.commands.at(-1)?.type, "steer");

  await controller.handle(message("/cancel"));
  assert.equal(runner.jobs[0]?.commands.at(-1)?.type, "cancel");
  assert.match(messenger.sent.at(-1)?.text ?? "", /Cancelled/);
});

test("reports a worker crash as a failed job", async () => {
  const messenger = new FakeMessenger();
  const runner = new InMemorySandboxRunner();
  const controller = new Controller(messenger, new FakeApprovals(), runner, { app: "/work/app" });

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
});
