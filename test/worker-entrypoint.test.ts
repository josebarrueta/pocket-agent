import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { resolve } from "node:path";
import test from "node:test";

const entrypoint = resolve("docker/worker/worker.mjs");
const piBinary = resolve("node_modules/.bin/pi");

function run(input = "", args: string[] = []) {
  return spawnSync(process.execPath, [entrypoint, ...args], {
    encoding: "utf8",
    input,
    env: { ...process.env, PI_BINARY: piBinary },
    timeout: 15_000,
  });
}

test("worker smoke test verifies Pi and reports its identity", () => {
  const result = run("", ["--smoke-test"]);
  assert.equal(result.status, 0, result.stderr);
  const output = JSON.parse(result.stdout);
  assert.equal(output.ok, true);
  assert.equal(output.piVersion, "0.87.1");
  assert.deepEqual(output.protocolVersions, [1]);
});

test("worker negotiates v1 and fails closed until Pi execution is wired", () => {
  const input = [
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
  const result = run(input);

  assert.equal(result.status, 0, result.stderr);
  const messages = result.stdout.trim().split("\n").map((line) => JSON.parse(line));
  assert.deepEqual(messages[0], { type: "hello", supportedVersions: [1] });
  assert.equal(messages[1].type, "failure");
  assert.equal(messages[1].code, "internal_error");
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
