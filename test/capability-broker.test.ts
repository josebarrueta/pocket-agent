import assert from "node:assert/strict";
import { request as httpRequest } from "node:http";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import {
  CapabilityBroker,
  CapabilityBrokerError,
  type CapabilityTool,
  type JsonValue,
} from "../src/capability-broker.js";
import type { ApprovalPort, ApprovalRequest } from "../src/types.js";

class FakeApprovals implements ApprovalPort {
  requests: ApprovalRequest[] = [];
  private resolve?: (answer: string) => void;
  async request(_conversationId: string, request: ApprovalRequest): Promise<string> {
    this.requests.push(request);
    return new Promise((resolve) => { this.resolve = resolve; });
  }
  answer(): boolean { return false; }
  cancelScope(): void { this.resolve?.("cancel"); }
  respond(answer: string): void { this.resolve?.(answer); }
}

function metadataTool(policy: "allow" | "ask" = "allow", calls: JsonValue[] = []): CapabilityTool {
  return {
    name: "workspace.read_metadata",
    description: "Read scoped workspace metadata",
    inputSchema: { type: "object", additionalProperties: false },
    policy,
    normalize(input, context) {
      assert.deepEqual(input, {});
      return { repository: context.repositoryScope };
    },
    async invoke(input) {
      calls.push(input);
      return { ok: true };
    },
  };
}

async function fixture(tools: CapabilityTool[], now = new Date("2026-01-01T00:00:00Z")) {
  const root = await mkdtemp(join(tmpdir(), "pocket-broker-"));
  const approvals = new FakeApprovals();
  let clock = now;
  const broker = new CapabilityBroker({
    socketPath: join(root, "transport", "mcp.sock"),
    auditPath: join(root, "audit", "capabilities.ndjson"),
    tools,
    approvals,
    now: () => clock,
  });
  await broker.start();
  return {
    root,
    broker,
    approvals,
    setNow(value: Date) { clock = value; },
    async close() { await broker.close(); await rm(root, { recursive: true, force: true }); },
  };
}

function lease(broker: CapabilityBroker, jobId: string, allowedTools = ["workspace.read_metadata"]) {
  return broker.issue({
    jobId,
    conversationId: `conversation-${jobId}`,
    repositoryScope: `repo-${jobId}`,
    allowedTools,
    expiresAt: new Date("2026-01-01T01:00:00Z"),
  });
}

async function expectCode(promise: Promise<unknown>, code: CapabilityBrokerError["code"]): Promise<void> {
  await assert.rejects(promise, (error: unknown) => error instanceof CapabilityBrokerError && error.code === code);
}

test("leases deny cross-job, expired, replayed, altered, and out-of-scope requests", async (t) => {
  const calls: JsonValue[] = [];
  const fx = await fixture([metadataTool("allow", calls)]);
  t.after(() => fx.close());
  const first = lease(fx.broker, "job-a");
  const second = lease(fx.broker, "job-b", []);

  await expectCode(fx.broker.call(first.credential, "job-b", "one", "workspace.read_metadata", {}), "unauthorized");
  await expectCode(fx.broker.call(second.credential, "job-b", "one", "workspace.read_metadata", {}), "forbidden");
  assert.deepEqual((await fx.broker.list(second.credential, "job-b")), []);

  assert.deepEqual(await fx.broker.call(first.credential, "job-a", "one", "workspace.read_metadata", {}), { ok: true });
  await expectCode(fx.broker.call(first.credential, "job-a", "one", "workspace.read_metadata", { changed: true }), "replayed");
  assert.equal(calls.length, 1);

  fx.setNow(new Date("2026-01-01T02:00:00Z"));
  await expectCode(fx.broker.call(first.credential, "job-a", "two", "workspace.read_metadata", {}), "expired");
});

test("approval binds normalized arguments and revocation wins approval races", async (t) => {
  const calls: JsonValue[] = [];
  const fx = await fixture([metadataTool("ask", calls)]);
  t.after(() => fx.close());
  const granted = lease(fx.broker, "job-a");

  const pending = fx.broker.call(granted.credential, "job-a", "nonce", "workspace.read_metadata", {});
  while (!fx.approvals.requests.length) await new Promise((resolve) => setTimeout(resolve, 0));
  assert.match(fx.approvals.requests[0]!.detail, /Arguments: sha256:[a-f0-9]{64}/);
  assert.deepEqual(fx.approvals.requests[0]!.operation, {
    tool: "workspace.read_metadata",
    argumentDigest: fx.approvals.requests[0]!.detail.match(/Arguments: (sha256:[a-f0-9]{64})/)?.[1],
    expiresAt: "2026-01-01T01:00:00.000Z",
    nonce: fx.approvals.requests[0]!.operation?.nonce,
  });
  assert.match(fx.approvals.requests[0]!.operation!.nonce, /^[A-Za-z0-9_-]{22}$/);
  assert.doesNotMatch(fx.approvals.requests[0]!.detail, /credential/i);
  granted.revoke();
  fx.approvals.respond("yes");
  await expectCode(pending, "unauthorized");
  assert.equal(calls.length, 0);
});

test("MCP transport authenticates over a Unix socket and lists only scoped tools", async (t) => {
  const fx = await fixture([metadataTool()]);
  t.after(() => fx.close());
  const granted = lease(fx.broker, "job-a");

  const response = await rpc(granted.socketPath, granted.credential, {
    jsonrpc: "2.0",
    id: 1,
    method: "tools/list",
    params: { _meta: { "pocket-agent/job-id": "job-a" } },
  });
  assert.equal(response.status, 200);
  assert.deepEqual((response.body as { result: { tools: Array<{ name: string }> } }).result.tools.map((tool) => tool.name), ["workspace.read_metadata"]);

  const denied = await rpc(granted.socketPath, granted.credential, {
    jsonrpc: "2.0",
    id: 2,
    method: "tools/list",
    params: { _meta: { "pocket-agent/job-id": "job-b" } },
  });
  assert.equal(denied.status, 401);
});

test("audit records contain identity and argument digests but not secret arguments or credentials", async (t) => {
  const secret = "super-secret-value";
  const tool: CapabilityTool = {
    name: "connector.perform",
    description: "Perform a typed connector operation",
    inputSchema: { type: "object" },
    policy: "allow",
    normalize(input) { return input as JsonValue; },
    async invoke() { return { ok: true }; },
  };
  const fx = await fixture([tool]);
  t.after(() => fx.close());
  const granted = lease(fx.broker, "job-a", [tool.name]);
  await fx.broker.call(granted.credential, "job-a", "one", tool.name, { token: secret });

  const audit = await readFile(join(fx.root, "audit", "capabilities.ndjson"), "utf8");
  assert.match(audit, /"jobId":"job-a"/);
  assert.match(audit, /"argumentDigest":"sha256:[a-f0-9]{64}"/);
  assert.doesNotMatch(audit, new RegExp(secret));
  assert.doesNotMatch(audit, new RegExp(granted.credential.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
});

test("lease call and output limits fail closed", async (t) => {
  const fx = await fixture([metadataTool()]);
  t.after(() => fx.close());
  const granted = fx.broker.issue({
    jobId: "job-a",
    conversationId: "conversation-a",
    repositoryScope: "repo-a",
    allowedTools: ["workspace.read_metadata"],
    expiresAt: new Date("2026-01-01T01:00:00Z"),
    maxCalls: 1,
    maxOutputBytes: 2,
  });
  await expectCode(fx.broker.call(granted.credential, "job-a", "one", "workspace.read_metadata", {}), "limit_exceeded");
  await expectCode(fx.broker.call(granted.credential, "job-a", "two", "workspace.read_metadata", {}), "limit_exceeded");
});

test("generic host execution capabilities are rejected", async () => {
  const root = await mkdtemp(join(tmpdir(), "pocket-broker-invalid-"));
  assert.throws(() => new CapabilityBroker({
    socketPath: join(root, "mcp.sock"),
    auditPath: join(root, "audit.ndjson"),
    approvals: new FakeApprovals(),
    tools: [{ ...metadataTool(), name: "host.exec" }],
  }), /forbidden/);
  await rm(root, { recursive: true, force: true });
});

function rpc(socketPath: string, credential: string, body: object): Promise<{ status: number; body: unknown }> {
  return new Promise((resolve, reject) => {
    const request = httpRequest({
      socketPath,
      path: "/mcp",
      method: "POST",
      headers: { authorization: `Bearer ${credential}`, "content-type": "application/json" },
    }, (response) => {
      const chunks: Buffer[] = [];
      response.on("data", (chunk: Buffer) => chunks.push(chunk));
      response.on("end", () => resolve({
        status: response.statusCode ?? 0,
        body: JSON.parse(Buffer.concat(chunks).toString("utf8")),
      }));
    });
    request.once("error", reject);
    request.end(JSON.stringify(body));
  });
}
