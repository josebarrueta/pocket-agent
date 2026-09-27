import assert from "node:assert/strict";
import test from "node:test";
import { Controller } from "../src/controller.js";
import type {
  AgentFactory,
  AgentRun,
  ApprovalPort,
  IncomingMessage,
  Messenger,
} from "../src/types.js";

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

class FakeRun implements AgentRun {
  isRunning = false;
  prompts: string[] = [];
  steers: string[] = [];
  cancelled = false;
  constructor(readonly id: string) {}
  async start(prompt: string): Promise<string> {
    this.isRunning = true;
    this.prompts.push(prompt);
    this.isRunning = false;
    return "done";
  }
  async steer(message: string): Promise<void> { this.steers.push(message); }
  async cancel(): Promise<void> { this.cancelled = true; this.isRunning = false; }
  async dispose(): Promise<void> {}
}

class FakeFactory implements AgentFactory {
  runs: FakeRun[] = [];
  async create(options: Parameters<AgentFactory["create"]>[0]): Promise<AgentRun> {
    const run = new FakeRun(options.id);
    this.runs.push(run);
    return run;
  }
}

function message(text: string, conversationId = "operator"): IncomingMessage {
  return { id: "1", conversationId, senderId: conversationId, text, receivedAt: new Date() };
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

test("starts a job only in an operator-configured repository", async () => {
  const messenger = new FakeMessenger();
  const approvals = new FakeApprovals();
  const factory = new FakeFactory();
  const controller = new Controller(messenger, approvals, factory, { app: "/work/app" });

  await controller.handle(message("/new app fix the login race"));
  await tick();

  assert.equal(factory.runs.length, 1);
  assert.deepEqual(factory.runs[0]?.prompts, ["fix the login race"]);
  assert.match(messenger.sent.at(-1)?.text ?? "", /done/);

  await controller.handle(message("/new unknown do something"));
  assert.match(messenger.sent.at(-1)?.text ?? "", /Unknown repo/);
});

test("bug command adds a reproduce, fix, and test instruction", async () => {
  const messenger = new FakeMessenger();
  const factory = new FakeFactory();
  const controller = new Controller(messenger, new FakeApprovals(), factory, { app: "/work/app" });

  await controller.handle(message("/bug app crashes on empty input"));
  await tick();

  assert.match(factory.runs[0]?.prompts[0] ?? "", /reproduce it/i);
  assert.match(factory.runs[0]?.prompts[0] ?? "", /run relevant tests/i);
});

test("jobs cannot be selected from a different conversation", async () => {
  const messenger = new FakeMessenger();
  const factory = new FakeFactory();
  const controller = new Controller(messenger, new FakeApprovals(), factory, { app: "/work/app" });

  await controller.handle(message("/new app task", "alice"));
  await tick();
  const id = factory.runs[0]!.id;
  await controller.handle(message(`/use ${id}`, "bob"));

  assert.equal(messenger.sent.at(-1)?.conversationId, "bob");
  assert.match(messenger.sent.at(-1)?.text ?? "", /No matching job/);
});
