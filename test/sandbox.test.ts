import assert from "node:assert/strict";
import test from "node:test";
import { InMemorySandboxRunner } from "../src/in-memory-sandbox.js";
import type { StartMessage, WorkerToHostMessage } from "../src/sandbox-protocol.js";
import { SandboxFailure, type JobSandboxSpec } from "../src/sandbox.js";

function spec(overrides: Partial<JobSandboxSpec> = {}): JobSandboxSpec {
  return {
    id: "job-1",
    workspacePath: "/work/app",
    conversationId: "operator",
    deadlineAt: new Date(Date.now() + 10_000),
    outputLimitBytes: 1_000,
    events: { status: async () => {} },
    ...overrides,
  };
}

function startCommand(commands: readonly unknown[]): StartMessage {
  const command = commands[0];
  assert.ok(command && typeof command === "object" && "type" in command && command.type === "start");
  return command as StartMessage;
}

function completion(start: StartMessage, output = "done"): WorkerToHostMessage {
  return {
    protocolVersion: 1,
    type: "completion",
    jobId: start.jobId,
    runId: start.runId,
    output,
  };
}

test("records versioned start and steer protocol messages", async () => {
  const runner = new InMemorySandboxRunner();
  const job = await runner.create(spec());
  const result = job.start("fix it");
  const start = startCommand(job.commands);

  assert.equal(start.jobId, "job-1");
  assert.equal(start.protocolVersion, 1);
  assert.equal(start.outputLimitBytes, 1_000);
  await job.steer("try the parser first");
  assert.deepEqual(job.commands.map((message) => message.type), ["start", "steer"]);

  await job.receive(completion(start));
  assert.equal(await result, "done");
  await job.dispose();
});

test("rejects a job at its deadline and asks the worker to cancel", async () => {
  const runner = new InMemorySandboxRunner();
  const job = await runner.create(spec({ deadlineAt: new Date(Date.now() - 1) }));
  const result = job.start("too late");

  await assert.rejects(result, (error: unknown) => error instanceof SandboxFailure && error.code === "deadline_exceeded");
  assert.deepEqual(job.commands.map((message) => message.type), ["start", "cancel"]);
});

test("turns a worker crash into a typed failure", async () => {
  const runner = new InMemorySandboxRunner();
  const job = await runner.create(spec());
  const result = job.start("task");
  const start = startCommand(job.commands);

  await job.receive({
    protocolVersion: 1,
    type: "failure",
    jobId: start.jobId,
    runId: start.runId,
    code: "worker_crash",
    message: "lost worker",
    retryable: true,
  });

  await assert.rejects(result, (error: unknown) =>
    error instanceof SandboxFailure && error.code === "worker_crash" && error.retryable,
  );
  await job.dispose();
});

test("accepts only one terminal event per run", async () => {
  const runner = new InMemorySandboxRunner();
  const job = await runner.create(spec());
  const result = job.start("task");
  const start = startCommand(job.commands);

  assert.equal(await job.receive(completion(start, "first")), true);
  assert.equal(await result, "first");
  assert.equal(await job.receive(completion(start, "duplicate")), false);
  await job.dispose();
});

test("cancellation and disposal are idempotent and disposal drops later events", async () => {
  let statuses = 0;
  const runner = new InMemorySandboxRunner();
  const job = await runner.create(spec({ events: { status: async () => { statuses += 1; } } }));
  const result = job.start("task");
  const start = startCommand(job.commands);

  await job.cancel();
  await job.cancel();
  await assert.rejects(result, /cancelled/);
  assert.equal(job.commands.filter((message) => message.type === "cancel").length, 1);

  await job.dispose();
  await job.dispose();
  assert.equal(await job.receive({
    protocolVersion: 1,
    type: "status",
    jobId: start.jobId,
    runId: start.runId,
    message: "late status",
  }), false);
  assert.equal(statuses, 0);
});

test("fails creation when worker and host protocol versions do not overlap", async () => {
  const runner = new InMemorySandboxRunner([99]);
  await assert.rejects(runner.create(spec()), /No compatible sandbox protocol version/);
});
