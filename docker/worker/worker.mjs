#!/usr/bin/env node
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { request as httpRequest } from "node:http";
import { mkdir } from "node:fs/promises";
import { createInterface } from "node:readline";

const PROTOCOL_VERSIONS = [1];
const MAX_MESSAGE_BYTES = 1024 * 1024 + 4096;
const MAX_CAPABILITY_RESPONSE_BYTES = 8 * 1024 * 1024;
const MAX_ERROR_BYTES = 8 * 1024;
const EX_USAGE = 64;
const WORKSPACE = "/workspace";
const AGENT_DIR = "/tmp/home/.pi/agent";

let negotiated = false;
let protocolVersion;
let sessionPromise;
let active;
let disposed = false;
let outputBytes = 0;
const pendingApprovals = new Map();

function write(message) {
  const line = `${JSON.stringify(message)}\n`;
  outputBytes += Buffer.byteLength(line, "utf8");
  const limit = (active?.outputLimitBytes ?? 64 * 1024) + 1024 * 1024;
  if (outputBytes > limit) failProtocol("worker protocol output exceeded its limit");
  process.stdout.write(line);
}

function failProtocol(message) {
  process.stderr.write(`worker protocol error: ${message}\n`);
  void disposeSession().finally(() => process.exit(EX_USAGE));
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOnlyKeys(value, keys) {
  return Object.keys(value).every((key) => keys.includes(key));
}

function nonEmptyString(value, maximum = MAX_MESSAGE_BYTES) {
  return typeof value === "string" && value.length > 0 && Buffer.byteLength(value, "utf8") <= maximum;
}

function validIdentity(message) {
  return nonEmptyString(message.jobId, 256) && nonEmptyString(message.runId, 256);
}

function smokeTest() {
  const piBinary = process.env.PI_BINARY ?? "/opt/worker/node_modules/.bin/pi";
  const result = spawnSync(piBinary, ["--version"], { encoding: "utf8", timeout: 10_000 });
  if (result.error || result.status !== 0) {
    const detail = (result.error?.message ?? result.stderr.trim()) || `Pi exited ${result.status}`;
    process.stderr.write(`worker smoke test failed: ${detail}\n`);
    process.exitCode = 1;
    return;
  }
  write({
    ok: true,
    uid: typeof process.getuid === "function" ? process.getuid() : null,
    gid: typeof process.getgid === "function" ? process.getgid() : null,
    piVersion: result.stdout.trim(),
    protocolVersions: PROTOCOL_VERSIONS,
  });
}

async function createSession() {
  if (process.env.POCKET_AGENT_DISABLE_MODEL === "1") throw new Error("model execution disabled for protocol test");
  await mkdir(AGENT_DIR, { recursive: true, mode: 0o700 });
  const { Type } = await import("typebox");
  const {
    createAgentSession,
    DefaultResourceLoader,
    ModelRuntime,
    SessionManager,
    SettingsManager,
  } = await import("@earendil-works/pi-coding-agent");

  const thinking = process.env.POCKET_AGENT_THINKING ?? "medium";
  const settingsManager = SettingsManager.inMemory({
    defaultThinkingLevel: thinking,
    enableAnalytics: false,
    enableInstallTelemetry: false,
    defaultTools: ["read", "bash", "edit", "write"],
  }, { projectTrusted: false });
  const permissions = parsePermissions();
  const capabilityToolNames = [];
  const approvalExtension = (pi) => {
    pi.on("tool_call", async (event) => {
      const decision = decisionFor(event.toolName, permissions);
      if (decision === "allow") return;
      if (decision === "deny") return { block: true, reason: `${event.toolName} is denied by policy` };
      const answer = await requestApproval({
        kind: "agent-tool",
        title: `Allow Pi tool ${event.toolName}?`,
        detail: JSON.stringify(event.input, null, 2).slice(0, MAX_ERROR_BYTES),
        choices: ["yes", "no"],
      });
      if (answer !== "yes") return { block: true, reason: "Denied by operator" };
    });
  };
  const capabilityExtension = async (pi) => {
    if (!process.env.POCKET_AGENT_MCP_SOCKET || !process.env.POCKET_AGENT_MCP_CREDENTIAL || !process.env.POCKET_AGENT_JOB_ID) return;
    const listed = await capabilityRpc("tools/list", {});
    for (const tool of listed.tools ?? []) {
      const localName = String(tool.name).replace(/[^A-Za-z0-9_-]/g, "_");
      if (capabilityToolNames.includes(localName)) throw new Error(`Capability tool name collision: ${localName}`);
      capabilityToolNames.push(localName);
      pi.registerTool({
        name: localName,
        label: String(tool.name),
        description: String(tool.description),
        promptSnippet: `Use ${localName} for the scoped ${tool.name} capability.`,
        parameters: Type.Unsafe(tool.inputSchema),
        async execute(_toolCallId, params, signal) {
          const result = await capabilityRpc("tools/call", {
            name: tool.name,
            arguments: params,
            _meta: {
              "pocket-agent/job-id": process.env.POCKET_AGENT_JOB_ID,
              "pocket-agent/request-id": randomUUID(),
            },
          }, signal);
          const text = result.content?.find((item) => item.type === "text")?.text ?? JSON.stringify(result.structuredContent ?? null);
          return { content: [{ type: "text", text }], details: result.structuredContent };
        },
      });
    }
  };
  const resourceLoader = new DefaultResourceLoader({
    cwd: WORKSPACE,
    agentDir: AGENT_DIR,
    settingsManager,
    noExtensions: true,
    noSkills: true,
    noPromptTemplates: true,
    noThemes: true,
    extensionFactories: [approvalExtension, capabilityExtension],
  });
  await resourceLoader.reload();
  const extensionErrors = resourceLoader.getExtensions().errors;
  if (extensionErrors.length) throw new Error(`Worker extension initialization failed: ${extensionErrors[0].error}`);

  const modelRuntime = await ModelRuntime.create({
    authPath: `${AGENT_DIR}/auth.json`,
    modelsPath: `${AGENT_DIR}/models.json`,
  });
  let model;
  const configuredModel = process.env.POCKET_AGENT_MODEL;
  if (configuredModel) {
    const separator = configuredModel.indexOf("/");
    if (separator < 1 || separator === configuredModel.length - 1) throw new Error("POCKET_AGENT_MODEL must be provider/model-id");
    model = modelRuntime.getModel(configuredModel.slice(0, separator), configuredModel.slice(separator + 1));
    if (!model) throw new Error(`Unknown configured model ${configuredModel}`);
  } else {
    [model] = await modelRuntime.getAvailable();
    if (!model) throw new Error("No model is available inside the worker");
  }

  const created = await createAgentSession({
    cwd: WORKSPACE,
    agentDir: AGENT_DIR,
    modelRuntime,
    model,
    thinkingLevel: thinking,
    resourceLoader,
    settingsManager,
    sessionManager: SessionManager.inMemory(WORKSPACE),
    tools: ["read", "bash", "edit", "write", ...capabilityToolNames],
  });
  created.session.subscribe((event) => {
    if (disposed || !active || active.terminal) return;
    if (event.type === "tool_execution_start") {
      write({
        protocolVersion,
        type: "status",
        jobId: active.jobId,
        runId: active.runId,
        message: `tool:${String(event.toolName).slice(0, 128)}`,
      });
    }
  });
  return created.session;
}

function getSession() {
  sessionPromise ??= createSession();
  return sessionPromise;
}

async function startRun(message) {
  if (active && !active.terminal) return failProtocol("start received while a run is active");
  active = {
    jobId: message.jobId,
    runId: message.runId,
    outputLimitBytes: message.outputLimitBytes,
    terminal: false,
  };
  outputBytes = 0;
  try {
    const session = await getSession();
    if (active.runId !== message.runId || active.terminal) return;
    await session.prompt(message.prompt);
    if (active.runId !== message.runId || active.terminal) return;
    const output = session.getLastAssistantText() || "(Agent completed without a text response.)";
    if (Buffer.byteLength(output, "utf8") > message.outputLimitBytes) {
      terminalFailure(message, "output_limit_exceeded", "Worker output exceeded its byte limit", false);
    } else {
      active.terminal = true;
      write({ protocolVersion, type: "completion", jobId: message.jobId, runId: message.runId, output });
    }
  } catch (error) {
    if (active?.runId !== message.runId || active?.terminal) return;
    terminalFailure(message, "internal_error", safeError(error), false);
  }
}

async function steerRun(message) {
  if (!active || active.terminal || active.jobId !== message.jobId || active.runId !== message.runId) {
    return failProtocol("steer does not match the active run");
  }
  try {
    const session = await getSession();
    await session.steer(message.message);
  } catch (error) {
    if (!active.terminal) terminalFailure(message, "internal_error", safeError(error), false);
  }
}

async function cancelRun(message) {
  if (!active || active.terminal || active.jobId !== message.jobId || active.runId !== message.runId) return;
  active.terminal = true;
  try {
    const session = await getSession();
    await session.abort();
  } catch {
    // The host force-removes the sandbox after cancellation.
  }
}

function capabilityRpc(method, params, signal) {
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: randomUUID(),
    method,
    params: {
      ...params,
      _meta: {
        ...(params._meta ?? {}),
        "pocket-agent/job-id": process.env.POCKET_AGENT_JOB_ID,
      },
    },
  });
  return new Promise((resolve, reject) => {
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
      let bytes = 0;
      response.on("data", (chunk) => {
        bytes += chunk.length;
        if (bytes > MAX_CAPABILITY_RESPONSE_BYTES) request.destroy(new Error("Capability response exceeded its limit"));
        else chunks.push(chunk);
      });
      response.on("end", () => {
        try {
          const rpc = JSON.parse(Buffer.concat(chunks).toString("utf8"));
          if (response.statusCode !== 200 || rpc.error) reject(new Error(rpc.error?.message ?? `Capability broker returned HTTP ${response.statusCode}`));
          else resolve(rpc.result);
        } catch (error) { reject(error); }
      });
    });
    request.once("error", reject);
    if (signal) {
      if (signal.aborted) request.destroy(new Error("Capability call aborted"));
      else signal.addEventListener("abort", () => request.destroy(new Error("Capability call aborted")), { once: true });
    }
    request.end(body);
  });
}

function parsePermissions() {
  const fallback = { read: "allow", write: "ask", bash: "ask" };
  try {
    const parsed = JSON.parse(process.env.POCKET_AGENT_PERMISSIONS ?? "{}");
    for (const key of Object.keys(fallback)) {
      if (!["allow", "ask", "deny"].includes(parsed[key])) parsed[key] = fallback[key];
    }
    return parsed;
  } catch {
    return fallback;
  }
}

function decisionFor(toolName, permissions) {
  if (["read", "grep", "find", "ls"].includes(toolName)) return permissions.read;
  if (["edit", "write"].includes(toolName)) return permissions.write;
  if (toolName === "bash") return permissions.bash;
  if (toolName.startsWith("workspace_")) return "allow";
  return "deny";
}

function requestApproval(request) {
  if (!active || active.terminal) return Promise.reject(new Error("No active run for approval"));
  const requestId = randomUUID();
  write({
    protocolVersion,
    type: "approval_request",
    jobId: active.jobId,
    runId: active.runId,
    requestId,
    ...request,
  });
  return new Promise((resolve, reject) => pendingApprovals.set(requestId, { resolve, reject }));
}

function terminalFailure(message, code, detail, retryable) {
  if (!active || active.terminal) return;
  active.terminal = true;
  write({
    protocolVersion,
    type: "failure",
    jobId: message.jobId,
    runId: message.runId,
    code,
    message: detail,
    retryable,
  });
}

function safeError(error) {
  const message = error instanceof Error ? error.message : String(error);
  return `Pi run failed: ${Buffer.from(message, "utf8").subarray(0, MAX_ERROR_BYTES).toString("utf8")}`;
}

async function disposeSession() {
  if (disposed) return;
  disposed = true;
  for (const pending of pendingApprovals.values()) pending.reject(new Error("Worker disposed"));
  pendingApprovals.clear();
  try {
    const session = await sessionPromise;
    session?.dispose();
  } catch {
    // Initialization failures have no session to dispose.
  }
}

function validateMessage(message) {
  if (message.protocolVersion !== protocolVersion || !validIdentity(message)) return "message has invalid protocol version or identity";
  if (message.type === "start") {
    if (!hasOnlyKeys(message, ["protocolVersion", "type", "jobId", "runId", "prompt", "deadlineAt", "outputLimitBytes"]) ||
        !nonEmptyString(message.prompt) || !nonEmptyString(message.deadlineAt, 64) || Number.isNaN(Date.parse(message.deadlineAt)) ||
        !Number.isSafeInteger(message.outputLimitBytes) || message.outputLimitBytes <= 0) return "malformed start message";
    return;
  }
  if (message.type === "steer") {
    if (!hasOnlyKeys(message, ["protocolVersion", "type", "jobId", "runId", "message"]) || !nonEmptyString(message.message)) return "malformed steer message";
    return;
  }
  if (message.type === "cancel") {
    if (!hasOnlyKeys(message, ["protocolVersion", "type", "jobId", "runId", "reason"]) ||
        !["operator", "deadline", "dispose"].includes(message.reason)) return "malformed cancel message";
    return;
  }
  if (message.type === "approval_response") {
    if (!hasOnlyKeys(message, ["protocolVersion", "type", "jobId", "runId", "requestId", "answer"]) ||
        !nonEmptyString(message.requestId, 256) || typeof message.answer !== "string") return "malformed approval response";
    return;
  }
  return "unknown host message";
}

async function handleLine(line) {
  if (Buffer.byteLength(line, "utf8") > MAX_MESSAGE_BYTES) return failProtocol(`message exceeds ${MAX_MESSAGE_BYTES} bytes`);
  let message;
  try { message = JSON.parse(line); } catch { return failProtocol("message is not valid JSON"); }
  if (!isRecord(message)) return failProtocol("message must be a JSON object");

  if (!negotiated) {
    if (!hasOnlyKeys(message, ["type", "supportedVersions"]) || message.type !== "hello" ||
        !Array.isArray(message.supportedVersions) || !message.supportedVersions.every(Number.isSafeInteger)) {
      return failProtocol("first host message must be a valid hello");
    }
    protocolVersion = [...PROTOCOL_VERSIONS].reverse().find((version) => message.supportedVersions.includes(version));
    if (!protocolVersion) return failProtocol("no compatible protocol version");
    negotiated = true;
    return;
  }

  const invalid = validateMessage(message);
  if (invalid) return failProtocol(invalid);
  if (message.type === "start") void startRun(message);
  else if (message.type === "steer") void steerRun(message);
  else if (message.type === "cancel") void cancelRun(message);
  else {
    const pending = pendingApprovals.get(message.requestId);
    if (!pending || !active || active.jobId !== message.jobId || active.runId !== message.runId) return failProtocol("approval response does not match a pending request");
    pendingApprovals.delete(message.requestId);
    pending.resolve(message.answer);
  }
}

if (process.argv.length > 2) {
  if (process.argv.length === 3 && process.argv[2] === "--smoke-test") smokeTest();
  else {
    process.stderr.write("worker accepts only --smoke-test; job control is read from stdin\n");
    process.exitCode = EX_USAGE;
  }
} else {
  write({ type: "hello", supportedVersions: PROTOCOL_VERSIONS });
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  lines.on("line", (line) => void handleLine(line));
  lines.once("close", () => void disposeSession());
  process.once("SIGTERM", () => void disposeSession().finally(() => process.exit(0)));
  process.once("SIGINT", () => void disposeSession().finally(() => process.exit(0)));
}
