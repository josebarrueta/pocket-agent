import { spawn } from "node:child_process";
import { appendFile } from "node:fs/promises";
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
  if (message.prompt === "memory-pressure") {
    const allocations = [];
    setInterval(() => allocations.push(Buffer.alloc(16 * 1024 * 1024, 1)), 5);
    continue;
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
