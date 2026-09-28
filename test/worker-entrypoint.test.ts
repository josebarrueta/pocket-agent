import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import test from "node:test";

const entrypoint = resolve("docker/worker/worker.mjs");
const piBinary = resolve("test/fixtures/fake-pi");

function run(input = "", args: string[] = [], env: Record<string, string> = {}) {
  return spawnSync(process.execPath, [entrypoint, ...args], {
    encoding: "utf8",
    input,
    env: { ...process.env, PI_BINARY: piBinary, POCKET_AGENT_DISABLE_MODEL: "1", ...env },
    timeout: 15_000,
  });
}

function startInput(): string {
  return [
    JSON.stringify({ type: "hello", supportedVersions: [1] }),
    JSON.stringify({
      protocolVersion: 1,
      type: "start",
      jobId: "job-1",
      runId: "job-1:1",
      prompt: "test",
      deadlineAt: new Date(Date.now() + 60_000).toISOString(),
      outputLimitBytes: 1_000,
    }),
    "",
  ].join("\n");
}

test("worker smoke test verifies Pi and reports its identity", () => {
  const result = run("", ["--smoke-test"]);
  assert.equal(result.status, 0, result.stderr);
  const output = JSON.parse(result.stdout);
  assert.equal(output.ok, true);
  assert.equal(output.piVersion, "0.87.1");
  assert.deepEqual(output.protocolVersions, [1]);
});

test("worker negotiates v1 and reports bounded Pi initialization failures", () => {
  const result = run(startInput());

  assert.equal(result.status, 0, result.stderr);
  const messages = result.stdout.trim().split("\n").map((line) => JSON.parse(line));
  assert.deepEqual(messages[0], { type: "hello", supportedVersions: [1] });
  assert.equal(messages[1].type, "failure");
  assert.equal(messages[1].code, "internal_error");
});

test("worker fails closed when private broker or model transport is unavailable", () => {
  const broker = run(startInput(), [], {
    POCKET_AGENT_MCP_SOCKET: "/tmp/missing-broker.sock",
    POCKET_AGENT_MCP_CREDENTIAL: "broker-secret-must-not-leak",
    POCKET_AGENT_JOB_ID: "job-1",
  });
  assert.equal(broker.status, 0, broker.stderr);
  assert.match(broker.stdout, /"type":"failure"/);
  assert.doesNotMatch(`${broker.stdout}${broker.stderr}`, /broker-secret-must-not-leak/);

  const descriptor = JSON.stringify({
    provider: "fake", id: "fake", name: "Fake", reasoning: false, input: ["text"],
    cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 4096, maxTokens: 256,
  });
  const model = run(startInput(), [], {
    POCKET_AGENT_DISABLE_MODEL: "0",
    POCKET_AGENT_MODEL_SOCKET: "/tmp/missing-model.sock",
    POCKET_AGENT_MODEL_CREDENTIAL: "model-secret-must-not-leak",
    POCKET_AGENT_PROXY_MODEL: descriptor,
    POCKET_AGENT_MODEL: "fake/fake",
    POCKET_AGENT_JOB_ID: "job-1",
  });
  assert.equal(model.status, 0, model.stderr);
  assert.match(model.stdout, /"type":"failure"/);
  assert.doesNotMatch(`${model.stdout}${model.stderr}`, /model-secret-must-not-leak/);
});

test("worker refuses malformed and unsupported protocol input", () => {
  const malformed = run("not-json\n");
  assert.equal(malformed.status, 64);
  assert.match(malformed.stderr, /not valid JSON/);

  const unsupported = run(`${JSON.stringify({ type: "hello", supportedVersions: [99] })}\n`);
  assert.equal(unsupported.status, 64);
  assert.match(unsupported.stderr, /no compatible protocol version/);

  const extraField = run(`${JSON.stringify({ type: "hello", supportedVersions: [1], token: "must-not-pass" })}\n`);
  assert.equal(extraField.status, 64);
  assert.match(extraField.stderr, /valid hello/);
});

test("worker refuses command-line entrypoint overrides", () => {
  const result = run("", ["bash"]);
  assert.equal(result.status, 64);
  assert.match(result.stderr, /accepts only --smoke-test/);
});
