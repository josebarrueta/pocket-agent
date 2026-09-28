import { spawn } from "node:child_process";
import { appendFile, readFile } from "node:fs/promises";
import { createInterface } from "node:readline";

const send = (message) => process.stdout.write(`${JSON.stringify(message)}\n`);
send({ type: "hello", supportedVersions: [1] });

let negotiated = false;
let active;
const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  const message = JSON.parse(line);
  if (!negotiated) {
    if (message.type !== "hello" || !message.supportedVersions.includes(1)) process.exit(64);
    negotiated = true;
    continue;
  }
  if (message.type === "cancel") process.exit(0);
  if (message.type === "approval_response" && active) {
    await appendFile("/workspace/approval.txt", `${message.answer}\n`);
    send({ protocolVersion: 1, type: "completion", jobId: active.jobId, runId: active.runId, output: "approval handled" });
    active = undefined;
    continue;
  }
  if (message.type === "steer") {
    send({ protocolVersion: 1, type: "status", jobId: message.jobId, runId: message.runId, message: "steered" });
    if (active) {
      send({ protocolVersion: 1, type: "completion", jobId: active.jobId, runId: active.runId, output: "completed after steer" });
      active = undefined;
    }
    continue;
  }
  if (message.type !== "start") process.exit(64);
  if (message.prompt === "crash") process.exit(23);
  if (message.prompt === "hang") { active = message; continue; }
  if (message.prompt === "approval") {
    active = message;
    send({
      protocolVersion: 1,
      type: "approval_request",
      jobId: message.jobId,
      runId: message.runId,
      requestId: "request-1",
      kind: "agent-tool",
      title: "Allow test tool?",
      detail: "test operation",
      choices: ["yes", "no"],
    });
    continue;
  }
  if (message.prompt === "memory-pressure") {
    const allocations = [];
    setInterval(() => allocations.push(Buffer.alloc(16 * 1024 * 1024, 1)), 5);
    continue;
  }
  if (message.prompt.startsWith("probe-isolation:")) {
    const hostPath = message.prompt.slice("probe-isolation:".length);
    let hostFileAccessible = true;
    try { await readFile(hostPath); } catch { hostFileAccessible = false; }
    let dockerSocketAccessible = true;
    try { await readFile("/var/run/docker.sock"); } catch { dockerSocketAccessible = false; }
    await appendFile("/workspace/isolation.json", JSON.stringify({
      inheritedSecret: process.env.POCKET_AGENT_HOST_TEST_SECRET ?? null,
      hostFileAccessible,
      dockerSocketAccessible,
    }));
  }
  if (message.prompt === "fork-pressure") {
    for (let index = 0; index < 256; index += 1) {
      const child = spawn("sleep", ["10"]);
      child.on("error", () => {});
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  await appendFile("/workspace/worker.txt", `${message.prompt}\n`);
  send({
    protocolVersion: 1,
    type: "completion",
    jobId: message.jobId,
    runId: message.runId,
    output: `completed ${message.prompt}`,
  });
}
