import { spawn } from "node:child_process";
import { appendFile, readFile, rm, writeFile } from "node:fs/promises";
import { request as httpRequest } from "node:http";
import { createConnection } from "node:net";
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
  if (message.type === "cancel") {
    if (active?.prompt === "ignore-cancel") continue;
    process.exit(0);
  }
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
  if (message.prompt === "hang" || message.prompt === "ignore-cancel") { active = message; continue; }
  if (message.prompt === "huge-output") {
    send({ protocolVersion: 1, type: "completion", jobId: message.jobId, runId: message.runId, output: "x".repeat(message.outputLimitBytes + 1) });
    continue;
  }
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
  if (message.prompt === "broker-list") {
    const body = JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "tools/list",
      params: { _meta: { "pocket-agent/job-id": process.env.POCKET_AGENT_JOB_ID } },
    });
    const brokerResult = await new Promise((resolve, reject) => {
      const request = httpRequest({
        socketPath: process.env.POCKET_AGENT_MCP_SOCKET,
        path: "/mcp",
        method: "POST",
        headers: {
          authorization: `Bearer ${process.env.POCKET_AGENT_MCP_CREDENTIAL}`,
          "content-type": "application/json",
          "content-length": Buffer.byteLength(body),
        },
      }, (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => resolve(JSON.parse(Buffer.concat(chunks).toString("utf8"))));
      });
      request.once("error", reject);
      request.end(body);
    });
    await appendFile("/workspace/broker.json", JSON.stringify(brokerResult));
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
  if (message.prompt.startsWith("probe-boundary:")) {
    const probe = JSON.parse(Buffer.from(message.prompt.slice("probe-boundary:".length), "base64url").toString("utf8"));
    const readablePaths = [];
    for (const path of probe.paths) {
      try { await readFile(path); readablePaths.push(path); } catch {}
    }
    const connect = (host, port) => new Promise((resolve) => {
      const socket = createConnection({ host, port });
      const done = (value) => { socket.destroy(); resolve(value); };
      socket.setTimeout(500, () => done(false));
      socket.once("connect", () => done(true));
      socket.once("error", () => done(false));
    });
    const boundary = {
      readablePaths,
      inheritedSecrets: Object.keys(process.env).filter((key) => key.startsWith("POCKET_AGENT_HOST_")),
      internetReachable: await connect("1.1.1.1", 53),
      hostPortReachable: await connect("172.17.0.1", probe.hostPort),
    };
    await writeFile("/workspace/boundary.json", JSON.stringify(boundary));
    send({ protocolVersion: 1, type: "completion", jobId: message.jobId, runId: message.runId, output: JSON.stringify(boundary) });
    continue;
  }
  if (message.prompt === "disk-pressure") {
    let bounded = false;
    try { await writeFile("/workspace/fill", Buffer.alloc(32 * 1024 * 1024, 1)); } catch (error) { bounded = error?.code === "ENOSPC"; }
    await rm("/workspace/fill", { force: true });
    await writeFile("/workspace/disk.json", JSON.stringify({ bounded }));
    send({ protocolVersion: 1, type: "completion", jobId: message.jobId, runId: message.runId, output: JSON.stringify({ bounded }) });
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
