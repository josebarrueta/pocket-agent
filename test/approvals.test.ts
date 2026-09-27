import assert from "node:assert/strict";
import test from "node:test";
import { MessageApprovalBroker } from "../src/approvals.js";
import type { Messenger } from "../src/types.js";

class FakeMessenger implements Messenger {
  sent: string[] = [];
  async start(): Promise<void> {}
  async send(_conversationId: string, text: string): Promise<void> { this.sent.push(text); }
  async close(): Promise<void> {}
}

test("approval answers are scoped to their conversation and choices", async () => {
  const messenger = new FakeMessenger();
  const broker = new MessageApprovalBroker(messenger);
  const pending = broker.request("alice", {
    kind: "agent-tool",
    scopeId: "job-1",
    title: "Allow bash?",
    detail: "npm test",
    choices: ["yes", "no"],
  });
  await new Promise((resolve) => setTimeout(resolve, 0));

  assert.equal(broker.answer("bob", "1", "yes"), false);
  assert.equal(broker.answer("alice", "1", "maybe"), false);
  assert.equal(broker.answer("alice", "1", "YES"), true);
  assert.equal(await pending, "yes");
  assert.match(messenger.sent[0] ?? "", /\/answer 1/);
});

test("cancelling a conversation releases pending requests", async () => {
  const broker = new MessageApprovalBroker(new FakeMessenger());
  const pending = broker.request("alice", {
    kind: "question",
    scopeId: "job-1",
    title: "Question",
    detail: "Continue?",
  });
  await new Promise((resolve) => setTimeout(resolve, 0));
  broker.cancelScope("alice", "job-1");
  assert.equal(await pending, "cancel");
});
