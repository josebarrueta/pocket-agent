#!/usr/bin/env node
import { spawnSync } from "node:child_process";

const PROTOCOL_VERSIONS = [1];
const MAX_MESSAGE_BYTES = 64 * 1024;
const EX_USAGE = 64;

function write(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function protocolError(message) {
  process.stderr.write(`worker protocol error: ${message}\n`);
  process.exit(EX_USAGE);
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

if (process.argv.length > 2) {
  if (process.argv.length === 3 && process.argv[2] === "--smoke-test") smokeTest();
  else {
    process.stderr.write("worker accepts only --smoke-test; job control is read from stdin\n");
    process.exitCode = EX_USAGE;
  }
} else {
  write({ type: "hello", supportedVersions: PROTOCOL_VERSIONS });

  let negotiated = false;
  let buffer = "";
  process.stdin.setEncoding("utf8");

  for await (const chunk of process.stdin) {
    buffer += chunk;
    if (Buffer.byteLength(buffer, "utf8") > MAX_MESSAGE_BYTES && !buffer.includes("\n")) {
      protocolError(`message exceeds ${MAX_MESSAGE_BYTES} bytes`);
      process.stdin.destroy();
      break;
    }

    let newline;
    while (process.exitCode === undefined && (newline = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newline);
      buffer = buffer.slice(newline + 1);
      if (!handleLine(line)) process.stdin.destroy();
    }
  }

  if (process.exitCode === undefined && buffer.length > 0) handleLine(buffer);

  function handleLine(line) {
    if (Buffer.byteLength(line, "utf8") > MAX_MESSAGE_BYTES) {
      protocolError(`message exceeds ${MAX_MESSAGE_BYTES} bytes`);
      return false;
    }

    let message;
    try {
      message = JSON.parse(line);
    } catch {
      protocolError("message is not valid JSON");
      return false;
    }
    if (!isRecord(message)) {
      protocolError("message must be a JSON object");
      return false;
    }

    if (!negotiated) {
      if (!hasOnlyKeys(message, ["type", "supportedVersions"]) || message.type !== "hello" ||
          !Array.isArray(message.supportedVersions) ||
          !message.supportedVersions.every(Number.isSafeInteger)) {
        protocolError("first host message must be a valid hello");
        return false;
      }
      if (!message.supportedVersions.includes(1)) {
        protocolError("no compatible protocol version");
        return false;
      }
      negotiated = true;
      return true;
    }

    if (message.protocolVersion !== 1) {
      protocolError("message uses an unsupported protocol version");
      return false;
    }
    if (message.type !== "start") {
      protocolError("worker is not running a job that accepts this message");
      return false;
    }
    if (!hasOnlyKeys(message, ["protocolVersion", "type", "jobId", "runId", "prompt", "deadlineAt", "outputLimitBytes"]) ||
        !validIdentity(message) || !nonEmptyString(message.prompt) ||
        !nonEmptyString(message.deadlineAt, 64) || Number.isNaN(Date.parse(message.deadlineAt)) ||
        !Number.isSafeInteger(message.outputLimitBytes) || message.outputLimitBytes <= 0) {
      protocolError("malformed start message");
      return false;
    }

    // Issue #5 replaces this fail-closed placeholder with Pi execution. Keeping
    // it terminal prevents this image revision from hanging or running input by accident.
    write({
      protocolVersion: 1,
      type: "failure",
      jobId: message.jobId,
      runId: message.runId,
      code: "internal_error",
      message: "Pi execution is not enabled in this worker image revision",
      retryable: false,
    });
    return true;
  }
}
