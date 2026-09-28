import { createHash, randomBytes } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { appendFile, chmod, mkdir, rm } from "node:fs/promises";
import { dirname } from "node:path";
import type { ApprovalPort } from "./types.js";

const MAX_REQUEST_BYTES = 1024 * 1024;
const MCP_PROTOCOL_VERSION = "2025-06-18";
const FORBIDDEN_TOOLS = new Set(["exec", "shell", "command", "host.exec", "host.shell"]);

export type CapabilityPolicy = "allow" | "ask" | "deny";
export type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };

export interface CapabilityContext {
  jobId: string;
  repositoryScope: string;
}

export interface CapabilityTool {
  name: string;
  description: string;
  inputSchema: { readonly [key: string]: JsonValue };
  policy: CapabilityPolicy;
  normalize(input: unknown, context: CapabilityContext): JsonValue;
  invoke(input: JsonValue, context: CapabilityContext): Promise<JsonValue>;
}

export interface CapabilityLeaseRequest {
  jobId: string;
  conversationId: string;
  repositoryScope: string;
  allowedTools: readonly string[];
  expiresAt: Date;
  maxCalls?: number;
  maxOutputBytes?: number;
}

export interface CapabilityLease {
  socketDirectory: string;
  socketPath: string;
  credential: string;
  revoke(): void;
}

export interface CapabilityLeaseIssuer {
  issue(request: CapabilityLeaseRequest): CapabilityLease;
}

export interface CapabilityBrokerOptions {
  socketPath: string;
  auditPath: string;
  tools: readonly CapabilityTool[];
  approvals: ApprovalPort;
  now?: () => Date;
  defaultMaxCalls?: number;
  defaultMaxOutputBytes?: number;
}

interface LeaseState {
  credentialHash: string;
  jobId: string;
  conversationId: string;
  repositoryScope: string;
  allowedTools: ReadonlySet<string>;
  expiresAt: number;
  maxCalls: number;
  maxOutputBytes: number;
  calls: number;
  revoked: boolean;
  usedRequestIds: Set<string>;
}

interface RpcRequest {
  jsonrpc: "2.0";
  id?: string | number | null;
  method: string;
  params?: unknown;
}

export class CapabilityBrokerError extends Error {
  constructor(message: string, readonly code: "unauthorized" | "forbidden" | "expired" | "replayed" | "invalid_request" | "limit_exceeded") {
    super(message);
    this.name = "CapabilityBrokerError";
  }
}

/** Authenticates, authorizes, approves, invokes, bounds, and audits host capabilities. */
export class CapabilityBroker implements CapabilityLeaseIssuer {
  private readonly tools = new Map<string, CapabilityTool>();
  private readonly leases = new Map<string, LeaseState>();
  private readonly now: () => Date;
  private readonly defaultMaxCalls: number;
  private readonly defaultMaxOutputBytes: number;
  private server?: Server;

  constructor(private readonly options: CapabilityBrokerOptions) {
    if (!options.socketPath.startsWith("/")) throw new Error("Capability broker socketPath must be absolute");
    if (!options.auditPath.startsWith("/")) throw new Error("Capability broker auditPath must be absolute");
    this.now = options.now ?? (() => new Date());
    this.defaultMaxCalls = options.defaultMaxCalls ?? 100;
    this.defaultMaxOutputBytes = options.defaultMaxOutputBytes ?? 256 * 1024;
    if (!Number.isSafeInteger(this.defaultMaxCalls) || this.defaultMaxCalls <= 0) throw new Error("defaultMaxCalls must be positive");
    if (!Number.isSafeInteger(this.defaultMaxOutputBytes) || this.defaultMaxOutputBytes <= 0) throw new Error("defaultMaxOutputBytes must be positive");
    for (const tool of options.tools) {
      validateTool(tool, this.tools);
      this.tools.set(tool.name, tool);
    }
  }

  async start(): Promise<void> {
    if (this.server) return;
    await mkdir(dirname(this.options.socketPath), { recursive: true, mode: 0o711 });
    await mkdir(dirname(this.options.auditPath), { recursive: true, mode: 0o700 });
    await rm(this.options.socketPath, { force: true });
    const server = createServer((request, response) => void this.handleHttp(request, response));
    this.server = server;
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(this.options.socketPath, () => {
        server.off("error", reject);
        resolve();
      });
    });
    await chmod(dirname(this.options.socketPath), 0o711);
    await chmod(this.options.socketPath, 0o666);
  }

  async close(): Promise<void> {
    const server = this.server;
    delete this.server;
    this.leases.clear();
    if (server) await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
    await rm(this.options.socketPath, { force: true });
  }

  issue(request: CapabilityLeaseRequest): CapabilityLease {
    if (!this.server) throw new Error("Capability broker is not started");
    if (!request.jobId || !request.repositoryScope || !request.conversationId) throw new Error("Capability lease identity is incomplete");
    if (request.expiresAt.getTime() <= this.now().getTime()) throw new Error("Capability lease expiry must be in the future");
    const allowedTools = new Set(request.allowedTools);
    for (const name of allowedTools) {
      if (!this.tools.has(name)) throw new Error(`Unknown capability ${name}`);
    }
    const credential = randomBytes(32).toString("base64url");
    const credentialHash = hashCredential(credential);
    const state: LeaseState = {
      credentialHash,
      jobId: request.jobId,
      conversationId: request.conversationId,
      repositoryScope: request.repositoryScope,
      allowedTools,
      expiresAt: request.expiresAt.getTime(),
      maxCalls: request.maxCalls ?? this.defaultMaxCalls,
      maxOutputBytes: request.maxOutputBytes ?? this.defaultMaxOutputBytes,
      calls: 0,
      revoked: false,
      usedRequestIds: new Set(),
    };
    if (!Number.isSafeInteger(state.maxCalls) || state.maxCalls <= 0) throw new Error("Capability lease maxCalls must be positive");
    if (!Number.isSafeInteger(state.maxOutputBytes) || state.maxOutputBytes <= 0) throw new Error("Capability lease maxOutputBytes must be positive");
    this.leases.set(credentialHash, state);
    let revoked = false;
    return {
      socketDirectory: dirname(this.options.socketPath),
      socketPath: this.options.socketPath,
      credential,
      revoke: () => {
        if (revoked) return;
        revoked = true;
        state.revoked = true;
        this.leases.delete(credentialHash);
        void this.audit(state, undefined, undefined, "revoked").catch(() => { /* revocation must remain synchronous */ });
      },
    };
  }

  async list(credential: string, claimedJobId: string): Promise<Array<Pick<CapabilityTool, "name" | "description" | "inputSchema">>> {
    const lease = this.authenticate(credential, claimedJobId);
    return [...lease.allowedTools].map((name) => {
      const tool = this.tools.get(name)!;
      return { name: tool.name, description: tool.description, inputSchema: tool.inputSchema };
    });
  }

  async call(credential: string, claimedJobId: string, requestId: string, toolName: string, input: unknown): Promise<JsonValue> {
    const lease = this.authenticate(credential, claimedJobId);
    if (!requestId || Buffer.byteLength(requestId, "utf8") > 256) throw new CapabilityBrokerError("Request ID is invalid", "invalid_request");
    if (lease.calls >= lease.maxCalls) throw new CapabilityBrokerError("Capability call limit exceeded", "limit_exceeded");
    if (lease.usedRequestIds.has(requestId)) throw new CapabilityBrokerError("Request ID was already used", "replayed");
    lease.usedRequestIds.add(requestId);
    lease.calls++;
    if (!lease.allowedTools.has(toolName)) {
      await this.audit(lease, toolName, undefined, "forbidden");
      throw new CapabilityBrokerError("Capability is outside this job scope", "forbidden");
    }
    const tool = this.tools.get(toolName)!;
    const context = { jobId: lease.jobId, repositoryScope: lease.repositoryScope };
    let normalized: JsonValue;
    try {
      normalized = tool.normalize(input, context);
      assertJsonValue(normalized);
    } catch (error) {
      await this.audit(lease, toolName, undefined, "invalid_arguments");
      throw new CapabilityBrokerError(error instanceof Error ? error.message : "Capability arguments are invalid", "invalid_request");
    }
    const digest = digestJson(normalized);
    if (tool.policy === "deny") {
      await this.audit(lease, toolName, digest, "denied");
      throw new CapabilityBrokerError("Capability is denied by policy", "forbidden");
    }
    if (tool.policy === "ask") {
      const approvalNonce = randomBytes(16).toString("base64url");
      const answer = await this.options.approvals.request(lease.conversationId, {
        kind: "mcp-tool",
        scopeId: lease.jobId,
        title: `Allow capability ${tool.name}?`,
        detail: `Repository: ${lease.repositoryScope}\nArguments: sha256:${digest}\nExpires: ${new Date(lease.expiresAt).toISOString()}`,
        choices: ["yes", "no"],
        operation: {
          tool: tool.name,
          argumentDigest: `sha256:${digest}`,
          expiresAt: new Date(lease.expiresAt).toISOString(),
          nonce: approvalNonce,
        },
      });
      // Re-authenticate after the asynchronous approval to make cancellation deterministic.
      this.authenticate(credential, claimedJobId);
      if (answer !== "yes") {
        await this.audit(lease, toolName, digest, "denied");
        throw new CapabilityBrokerError("Capability was denied by operator", "forbidden");
      }
    }
    try {
      const result = await tool.invoke(normalized, context);
      assertJsonValue(result);
      if (Buffer.byteLength(JSON.stringify(result), "utf8") > lease.maxOutputBytes) {
        throw new CapabilityBrokerError("Capability output limit exceeded", "limit_exceeded");
      }
      await this.audit(lease, toolName, digest, "allowed");
      return result;
    } catch (error) {
      await this.audit(lease, toolName, digest, "failed");
      throw error;
    }
  }

  private authenticate(credential: string, claimedJobId: string): LeaseState {
    const lease = this.leases.get(hashCredential(credential));
    if (!lease || lease.revoked) throw new CapabilityBrokerError("Capability credential is invalid or revoked", "unauthorized");
    if (lease.expiresAt <= this.now().getTime()) {
      lease.revoked = true;
      this.leases.delete(lease.credentialHash);
      throw new CapabilityBrokerError("Capability credential expired", "expired");
    }
    if (lease.jobId !== claimedJobId) throw new CapabilityBrokerError("Capability credential belongs to another job", "unauthorized");
    return lease;
  }

  private async audit(lease: LeaseState, tool: string | undefined, argumentDigest: string | undefined, outcome: string): Promise<void> {
    const record = {
      timestamp: this.now().toISOString(),
      jobId: lease.jobId,
      repositoryScope: lease.repositoryScope,
      ...(tool ? { tool } : {}),
      ...(argumentDigest ? { argumentDigest: `sha256:${argumentDigest}` } : {}),
      outcome,
    };
    await appendFile(this.options.auditPath, `${JSON.stringify(record)}\n`, { encoding: "utf8", mode: 0o600 });
    await chmod(this.options.auditPath, 0o600);
  }

  private async handleHttp(request: IncomingMessage, response: ServerResponse): Promise<void> {
    if (request.method !== "POST" || request.url !== "/mcp") return sendJson(response, 404, rpcError(null, -32601, "Not found"));
    try {
      const credential = parseBearer(request.headers.authorization);
      const body = await readBody(request);
      const rpc = parseRpcRequest(body);
      const params = isRecord(rpc.params) ? rpc.params : {};
      const meta = isRecord(params._meta) ? params._meta : {};
      const jobId = typeof meta["pocket-agent/job-id"] === "string" ? meta["pocket-agent/job-id"] : "";
      if (rpc.method === "initialize") {
        this.authenticate(credential, jobId);
        return sendJson(response, 200, { jsonrpc: "2.0", id: rpc.id ?? null, result: {
          protocolVersion: MCP_PROTOCOL_VERSION,
          capabilities: { tools: { listChanged: false } },
          serverInfo: { name: "pocket-agent-capability-broker", version: "0.1.0" },
        } });
      }
      if (rpc.method === "notifications/initialized") {
        this.authenticate(credential, jobId);
        response.writeHead(202).end();
        return;
      }
      if (rpc.method === "tools/list") {
        const tools = await this.list(credential, jobId);
        return sendJson(response, 200, { jsonrpc: "2.0", id: rpc.id ?? null, result: { tools } });
      }
      if (rpc.method === "tools/call") {
        const name = typeof params.name === "string" ? params.name : "";
        const requestId = typeof meta["pocket-agent/request-id"] === "string" ? meta["pocket-agent/request-id"] : "";
        const result = await this.call(credential, jobId, requestId, name, params.arguments);
        return sendJson(response, 200, { jsonrpc: "2.0", id: rpc.id ?? null, result: {
          content: [{ type: "text", text: JSON.stringify(result) }],
          structuredContent: result,
          isError: false,
        } });
      }
      return sendJson(response, 200, rpcError(rpc.id ?? null, -32601, "Method not found"));
    } catch (error) {
      const message = error instanceof CapabilityBrokerError ? error.message : "Invalid MCP request";
      const status = error instanceof CapabilityBrokerError && ["unauthorized", "expired"].includes(error.code) ? 401 : 400;
      return sendJson(response, status, rpcError(null, -32000, message));
    }
  }
}

function validateTool(tool: CapabilityTool, existing: ReadonlyMap<string, CapabilityTool>): void {
  if (!/^[a-z][a-z0-9_-]*(?:\.[a-z][a-z0-9_-]*)+$/.test(tool.name)) throw new Error(`Invalid namespaced capability name ${tool.name}`);
  if (FORBIDDEN_TOOLS.has(tool.name) || tool.name.endsWith(".exec") || tool.name.endsWith(".shell")) {
    throw new Error(`Generic execution capability is forbidden: ${tool.name}`);
  }
  if (existing.has(tool.name)) throw new Error(`Duplicate capability ${tool.name}`);
  if (!tool.description || !isRecord(tool.inputSchema)) throw new Error(`Capability metadata is invalid: ${tool.name}`);
}

function hashCredential(credential: string): string {
  return createHash("sha256").update(credential).digest("hex");
}

function digestJson(value: JsonValue): string {
  return createHash("sha256").update(canonicalJson(value)).digest("hex");
}

function canonicalJson(value: JsonValue): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value !== null && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key]!)}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

function assertJsonValue(value: unknown, depth = 0): asserts value is JsonValue {
  if (depth > 32) throw new Error("JSON value is too deeply nested");
  if (value === null || typeof value === "string" || typeof value === "boolean") return;
  if (typeof value === "number" && Number.isFinite(value)) return;
  if (Array.isArray(value)) {
    for (const item of value) assertJsonValue(item, depth + 1);
    return;
  }
  if (isRecord(value)) {
    for (const item of Object.values(value)) assertJsonValue(item, depth + 1);
    return;
  }
  throw new Error("Capability values must be bounded JSON data");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function parseBearer(header: string | undefined): string {
  if (!header?.startsWith("Bearer ") || header.length > 1024) throw new CapabilityBrokerError("Missing capability credential", "unauthorized");
  return header.slice(7);
}

async function readBody(request: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const chunk of request) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    size += buffer.length;
    if (size > MAX_REQUEST_BYTES) throw new CapabilityBrokerError("MCP request is too large", "limit_exceeded");
    chunks.push(buffer);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function parseRpcRequest(body: string): RpcRequest {
  let value: unknown;
  try { value = JSON.parse(body); } catch { throw new CapabilityBrokerError("Malformed JSON-RPC", "invalid_request"); }
  if (!isRecord(value) || value.jsonrpc !== "2.0" || typeof value.method !== "string") {
    throw new CapabilityBrokerError("Malformed JSON-RPC", "invalid_request");
  }
  return value as unknown as RpcRequest;
}

function rpcError(id: RpcRequest["id"], code: number, message: string): object {
  return { jsonrpc: "2.0", id: id ?? null, error: { code, message } };
}

function sendJson(response: ServerResponse, status: number, value: object): void {
  const body = JSON.stringify(value);
  response.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(body) });
  response.end(body);
}
