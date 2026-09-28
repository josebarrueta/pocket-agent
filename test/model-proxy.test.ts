import assert from "node:assert/strict";
import { request as httpRequest } from "node:http";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import type { AssistantMessageEvent } from "@earendil-works/pi-ai/compat";
import { ModelProxy, type ModelBackend, type ModelStreamRequest } from "../src/model-proxy.js";

const descriptor = {
  provider: "test-provider",
  id: "test-model",
  name: "Test Model",
  reasoning: false,
  input: ["text"] as const,
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 1000,
  maxTokens: 100,
};

function done(tokens = 5): AssistantMessageEvent {
  return { type: "done", reason: "stop", message: { usage: { totalTokens: tokens } } } as AssistantMessageEvent;
}

async function fixture(stream?: ModelBackend["stream"], options: Partial<ConstructorParameters<typeof ModelProxy>[0]> = {}) {
  const root = await mkdtemp(join(tmpdir(), "pocket-model-proxy-"));
  const requests: ModelStreamRequest[] = [];
  const backend: ModelBackend = {
    descriptor,
    async *stream(request, limits) {
      requests.push(request);
      if (stream) yield* stream(request, limits);
      else yield done();
    },
  };
  const proxy = new ModelProxy({
    socketPath: join(root, "transport", "model.sock"),
    auditPath: join(root, "audit.ndjson"),
    backend,
    requestTimeoutMs: 1_000,
    ...options,
  });
  await proxy.start();
  return { root, proxy, requests, async close() { await proxy.close(); await rm(root, { recursive: true, force: true }); } };
}

function lease(proxy: ModelProxy, overrides = {}) {
  return proxy.issue({ jobId: "job-a", expiresAt: new Date(Date.now() + 60_000), ...overrides });
}

function body(overrides: Record<string, unknown> = {}) {
  return {
    provider: descriptor.provider,
    model: descriptor.id,
    context: { messages: [] },
    options: { reasoning: "medium" },
    ...overrides,
  };
}

function streamRequest(socketPath: string, credential: string, jobId: string, payload: object) {
  return new Promise<{ status: number; lines: Array<Record<string, unknown>> }>((resolve, reject) => {
    const request = httpRequest({
      socketPath,
      path: "/v1/stream",
      method: "POST",
      headers: {
        authorization: `Bearer ${credential}`,
        "x-pocket-agent-job-id": jobId,
        "content-type": "application/json",
      },
    }, (response) => {
      const chunks: Buffer[] = [];
      response.on("data", (chunk: Buffer) => chunks.push(chunk));
      response.on("end", () => {
        const text = Buffer.concat(chunks).toString("utf8");
        const lines = response.headers["content-type"]?.includes("ndjson")
          ? text.trim().split("\n").filter(Boolean).map((line) => JSON.parse(line))
          : [JSON.parse(text)];
        resolve({ status: response.statusCode ?? 0, lines });
      });
    });
    request.once("error", reject);
    request.end(JSON.stringify(payload));
  });
}

test("streams only the configured provider/model and never forwards URLs, headers, or credentials", async (t) => {
  const fx = await fixture();
  t.after(() => fx.close());
  const granted = lease(fx.proxy);
  const result = await streamRequest(granted.socketPath, granted.credential, "job-a", body());
  assert.equal(result.status, 200);
  assert.equal((result.lines[0]!.event as { type: string }).type, "done");
  assert.deepEqual(fx.requests[0], body());

  assert.equal((await streamRequest(granted.socketPath, granted.credential, "job-a", body({ model: "other" }))).status, 400);
  assert.equal((await streamRequest(granted.socketPath, granted.credential, "job-a", { ...body(), url: "https://evil.invalid", headers: { authorization: "stolen" } })).status, 400);
  assert.equal((await streamRequest(granted.socketPath, granted.credential, "job-b", body())).status, 401);
  granted.revoke();
  assert.equal((await streamRequest(granted.socketPath, granted.credential, "job-a", body())).status, 401);

  const audit = await readFile(join(fx.root, "audit.ndjson"), "utf8");
  assert.match(audit, /"jobId":"job-a"/);
  assert.match(audit, /"tokens":5/);
  assert.doesNotMatch(audit, /messages|authorization|Bearer/);
  assert.doesNotMatch(audit, new RegExp(granted.credential.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
});

test("expired model leases fail closed before provider dispatch", async (t) => {
  let now = new Date("2026-01-01T00:00:00Z");
  const fx = await fixture(undefined, { now: () => now });
  t.after(() => fx.close());
  const granted = fx.proxy.issue({ jobId: "job-a", expiresAt: new Date("2026-01-01T00:01:00Z") });
  now = new Date("2026-01-01T00:02:00Z");
  const result = await streamRequest(granted.socketPath, granted.credential, "job-a", body());
  assert.equal(result.status, 401);
  assert.equal(fx.requests.length, 0);
});

test("redacts provider error events before they reach a worker", async (t) => {
  const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
  const fx = await fixture(async function* () {
    yield {
      type: "error",
      reason: "error",
      error: {
        role: "assistant", content: [], api: "fake", provider: "fake", model: "fake",
        usage, stopReason: "error", errorMessage: "secret provider token", diagnostics: [{ severity: "error", message: "secret detail" }], timestamp: Date.now(),
      },
    } as unknown as AssistantMessageEvent;
  });
  t.after(() => fx.close());
  const granted = lease(fx.proxy);
  const result = await streamRequest(granted.socketPath, granted.credential, "job-a", body());
  assert.equal(result.status, 200);
  assert.doesNotMatch(JSON.stringify(result), /secret|diagnostics/);
  assert.match(JSON.stringify(result), /Model provider request failed/);
});

test("enforces request, rate, concurrency, and token limits", async (t) => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  let calls = 0;
  const fx = await fixture(async function* () {
    calls++;
    if (calls === 1) await gate;
    yield done(calls === 1 ? 4 : 6);
  }, { defaultMaxRequests: 2, defaultMaxRequestsPerMinute: 2, defaultMaxTokensPerRequest: 5, defaultMaxTokensPerJob: 10 });
  t.after(() => fx.close());
  const granted = lease(fx.proxy);

  const first = streamRequest(granted.socketPath, granted.credential, "job-a", body());
  while (!calls) await new Promise((resolve) => setTimeout(resolve, 0));
  const concurrent = await streamRequest(granted.socketPath, granted.credential, "job-a", body());
  assert.equal(concurrent.status, 400);
  assert.match(String((concurrent.lines[0] as { error: string }).error), /Concurrent/);
  release();
  assert.equal((await first).status, 200);
  const limited = await streamRequest(granted.socketPath, granted.credential, "job-a", body());
  assert.match(String((limited.lines.at(-1) as { error: string }).error), /token limit/);
  const exhausted = await streamRequest(granted.socketPath, granted.credential, "job-a", body());
  assert.equal(exhausted.status, 400);
  assert.match(String((exhausted.lines[0] as { error: string }).error), /job limit|rate limit/);
});

test("times out providers and redacts provider errors", async (t) => {
  let providerAborted = false;
  const fx = await fixture(async function* (_request, options) {
    await new Promise<void>((resolve) => {
      options.signal.addEventListener("abort", () => { providerAborted = true; resolve(); }, { once: true });
    });
    throw new Error("upstream detail must be redacted");
  }, { requestTimeoutMs: 25 });
  t.after(() => fx.close());
  const granted = lease(fx.proxy);
  const result = await streamRequest(granted.socketPath, granted.credential, "job-a", body());
  assert.equal(providerAborted, true);
  assert.match(String((result.lines.at(-1) as { error: string }).error), /cancelled/);
  assert.doesNotMatch(JSON.stringify(result), /upstream detail/);

  const failed = await fixture(async function* () { throw new Error("provider credential leaked"); });
  t.after(() => failed.close());
  const failedLease = lease(failed.proxy);
  const providerError = await streamRequest(failedLease.socketPath, failedLease.credential, "job-a", body());
  assert.match(String((providerError.lines.at(-1) as { error: string }).error), /provider request failed/);
  assert.doesNotMatch(JSON.stringify(providerError), /credential leaked/);
});

test("lease revocation aborts an active provider stream", async (t) => {
  let started!: () => void;
  const didStart = new Promise<void>((resolve) => { started = resolve; });
  let aborted = false;
  const fx = await fixture(async function* (_request, options) {
    started();
    await new Promise<void>((resolve) => options.signal.addEventListener("abort", () => { aborted = true; resolve(); }, { once: true }));
  }, { requestTimeoutMs: 5_000 });
  t.after(() => fx.close());
  const granted = lease(fx.proxy);
  const pending = streamRequest(granted.socketPath, granted.credential, "job-a", body());
  await didStart;
  granted.revoke();
  await pending;
  assert.equal(aborted, true);
});

test("worker disconnect aborts the provider stream", async (t) => {
  let started!: () => void;
  const didStart = new Promise<void>((resolve) => { started = resolve; });
  let aborted = false;
  const fx = await fixture(async function* (_request, options) {
    started();
    await new Promise<void>((resolve) => options.signal.addEventListener("abort", () => { aborted = true; resolve(); }, { once: true }));
  }, { requestTimeoutMs: 5_000 });
  t.after(() => fx.close());
  const granted = lease(fx.proxy);
  const client = httpRequest({
    socketPath: granted.socketPath,
    path: "/v1/stream",
    method: "POST",
    headers: { authorization: `Bearer ${granted.credential}`, "x-pocket-agent-job-id": "job-a" },
  });
  client.on("error", () => {});
  client.end(JSON.stringify(body()));
  await didStart;
  client.destroy();
  for (let index = 0; index < 100 && !aborted; index++) await new Promise((resolve) => setTimeout(resolve, 5));
  assert.equal(aborted, true);
});
