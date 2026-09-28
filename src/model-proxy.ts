import { createHash, randomBytes } from "node:crypto";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import { appendFile, chmod, mkdir, rm } from "node:fs/promises";
import { dirname } from "node:path";
import {
  getModel,
  streamSimple,
  type AssistantMessageEvent,
  type Model,
  type SimpleStreamOptions,
  type TranscriptContext,
} from "@earendil-works/pi-ai/compat";

const MAX_REQUEST_BYTES = 8 * 1024 * 1024;
const MAX_STREAM_BYTES = 16 * 1024 * 1024;

export interface ProxyModelDescriptor {
  provider: string;
  id: string;
  name: string;
  reasoning: boolean;
  input: readonly ("text" | "image")[];
  cost: { input: number; output: number; cacheRead: number; cacheWrite: number };
  contextWindow: number;
  maxTokens: number;
  thinkingLevelMap?: Readonly<Record<string, string | null>>;
}

export interface ModelStreamRequest {
  provider: string;
  model: string;
  context: TranscriptContext;
  options?: Pick<SimpleStreamOptions, "reasoning" | "temperature" | "toolChoice">;
}

export interface ModelBackend {
  readonly descriptor: ProxyModelDescriptor;
  stream(request: ModelStreamRequest, options: { signal: AbortSignal; maxTokens: number; timeoutMs: number }): AsyncIterable<AssistantMessageEvent>;
}

export interface ModelLeaseRequest {
  jobId: string;
  expiresAt: Date;
  maxRequests?: number;
  maxRequestsPerMinute?: number;
  maxTokensPerRequest?: number;
  maxTokensPerJob?: number;
}

export interface ModelLease {
  socketDirectory: string;
  socketPath: string;
  credential: string;
  model: ProxyModelDescriptor;
  revoke(): void;
}

export interface ModelLeaseIssuer {
  issue(request: ModelLeaseRequest): ModelLease;
}

export interface ModelProxyOptions {
  socketPath: string;
  auditPath: string;
  backend: ModelBackend;
  now?: () => Date;
  requestTimeoutMs?: number;
  defaultMaxRequests?: number;
  defaultMaxRequestsPerMinute?: number;
  defaultMaxTokensPerRequest?: number;
  defaultMaxTokensPerJob?: number;
}

interface LeaseState {
  credentialHash: string;
  jobId: string;
  expiresAt: number;
  maxRequests: number;
  maxRequestsPerMinute: number;
  maxTokensPerRequest: number;
  maxTokensPerJob: number;
  usedTokens: number;
  requests: number;
  requestTimes: number[];
  active: boolean;
  revoked: boolean;
  abort?: AbortController;
}

export class ModelProxyError extends Error {
  constructor(message: string, readonly code: "unauthorized" | "expired" | "forbidden" | "rate_limited" | "limit_exceeded" | "invalid_request") {
    super(message);
    this.name = "ModelProxyError";
  }
}

/** A credential-hiding, single-model streaming proxy scoped to disposable jobs. */
export class ModelProxy implements ModelLeaseIssuer {
  private readonly leases = new Map<string, LeaseState>();
  private readonly now: () => Date;
  private readonly requestTimeoutMs: number;
  private readonly defaults: Required<Pick<ModelLeaseRequest, "maxRequests" | "maxRequestsPerMinute" | "maxTokensPerRequest" | "maxTokensPerJob">>;
  private server?: Server;

  constructor(private readonly options: ModelProxyOptions) {
    if (!options.socketPath.startsWith("/")) throw new Error("Model proxy socketPath must be absolute");
    if (!options.auditPath.startsWith("/")) throw new Error("Model proxy auditPath must be absolute");
    this.now = options.now ?? (() => new Date());
    this.requestTimeoutMs = options.requestTimeoutMs ?? 120_000;
    this.defaults = {
      maxRequests: options.defaultMaxRequests ?? 100,
      maxRequestsPerMinute: options.defaultMaxRequestsPerMinute ?? 10,
      maxTokensPerRequest: options.defaultMaxTokensPerRequest ?? Math.min(options.backend.descriptor.maxTokens, 32_000),
      maxTokensPerJob: options.defaultMaxTokensPerJob ?? 200_000,
    };
    for (const [name, value] of Object.entries(this.defaults)) {
      if (!Number.isSafeInteger(value) || value <= 0) throw new Error(`${name} must be a positive integer`);
    }
  }

  async start(): Promise<void> {
    if (this.server) return;
    await mkdir(dirname(this.options.socketPath), { recursive: true, mode: 0o711 });
    await mkdir(dirname(this.options.auditPath), { recursive: true, mode: 0o700 });
    await rm(this.options.socketPath, { force: true });
    const server = createServer((request, response) => void this.handle(request, response));
    this.server = server;
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(this.options.socketPath, () => { server.off("error", reject); resolve(); });
    });
    await chmod(dirname(this.options.socketPath), 0o711);
    await chmod(this.options.socketPath, 0o666);
  }

  async close(): Promise<void> {
    const server = this.server;
    delete this.server;
    for (const lease of this.leases.values()) lease.abort?.abort(new Error("Model proxy closed"));
    this.leases.clear();
    if (server) {
      const closed = new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
      server.closeAllConnections();
      await closed;
    }
    await rm(this.options.socketPath, { force: true });
  }

  issue(request: ModelLeaseRequest): ModelLease {
    if (!this.server) throw new Error("Model proxy is not started");
    if (!request.jobId || request.expiresAt.getTime() <= this.now().getTime()) throw new Error("Model lease identity or expiry is invalid");
    const credential = randomBytes(32).toString("base64url");
    const credentialHash = hashCredential(credential);
    const state: LeaseState = {
      credentialHash,
      jobId: request.jobId,
      expiresAt: request.expiresAt.getTime(),
      maxRequests: request.maxRequests ?? this.defaults.maxRequests,
      maxRequestsPerMinute: request.maxRequestsPerMinute ?? this.defaults.maxRequestsPerMinute,
      maxTokensPerRequest: request.maxTokensPerRequest ?? this.defaults.maxTokensPerRequest,
      maxTokensPerJob: request.maxTokensPerJob ?? this.defaults.maxTokensPerJob,
      usedTokens: 0,
      requests: 0,
      requestTimes: [],
      active: false,
      revoked: false,
    };
    for (const value of [state.maxRequests, state.maxRequestsPerMinute, state.maxTokensPerRequest, state.maxTokensPerJob]) {
      if (!Number.isSafeInteger(value) || value <= 0) throw new Error("Model lease limits must be positive integers");
    }
    this.leases.set(credentialHash, state);
    let revoked = false;
    return {
      socketDirectory: dirname(this.options.socketPath),
      socketPath: this.options.socketPath,
      credential,
      model: this.options.backend.descriptor,
      revoke: () => {
        if (revoked) return;
        revoked = true;
        state.revoked = true;
        state.abort?.abort(new Error("Model lease revoked"));
        this.leases.delete(credentialHash);
      },
    };
  }

  private authenticate(credential: string, jobId: string): LeaseState {
    const lease = this.leases.get(hashCredential(credential));
    if (!lease || lease.revoked || lease.jobId !== jobId) throw new ModelProxyError("Model credential is invalid or revoked", "unauthorized");
    if (lease.expiresAt <= this.now().getTime()) {
      lease.revoked = true;
      this.leases.delete(lease.credentialHash);
      throw new ModelProxyError("Model credential expired", "expired");
    }
    return lease;
  }

  private begin(lease: LeaseState): void {
    const now = this.now().getTime();
    lease.requestTimes = lease.requestTimes.filter((time) => time > now - 60_000);
    if (lease.active) throw new ModelProxyError("Concurrent model requests are not allowed", "rate_limited");
    if (lease.requests >= lease.maxRequests || lease.usedTokens >= lease.maxTokensPerJob) {
      throw new ModelProxyError("Model job limit exceeded", "limit_exceeded");
    }
    if (lease.requestTimes.length >= lease.maxRequestsPerMinute) throw new ModelProxyError("Model rate limit exceeded", "rate_limited");
    lease.active = true;
    lease.requests++;
    lease.requestTimes.push(now);
  }

  private async handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    if (request.method !== "POST" || request.url !== "/v1/stream") return sendError(response, 404, "Not found");
    let lease: LeaseState | undefined;
    let outcome = "failed";
    let requestTokens = 0;
    const abort = new AbortController();
    const timeout = setTimeout(() => abort.abort(new Error("Model request timed out")), this.requestTimeoutMs);
    request.once("aborted", () => abort.abort(new Error("Worker cancelled model request")));
    response.once("close", () => { if (!response.writableEnded) abort.abort(new Error("Worker disconnected")); });
    try {
      const credential = parseBearer(request.headers.authorization);
      const jobId = singleHeader(request.headers["x-pocket-agent-job-id"]);
      lease = this.authenticate(credential, jobId);
      this.begin(lease);
      lease.abort = abort;
      const body = parseRequest(await readBody(request));
      const descriptor = this.options.backend.descriptor;
      if (body.provider !== descriptor.provider || body.model !== descriptor.id) {
        throw new ModelProxyError("Provider or model is outside this job scope", "forbidden");
      }
      await this.audit(lease, "started", 0);
      response.writeHead(200, { "content-type": "application/x-ndjson", "cache-control": "no-store" });
      let outputBytes = 0;
      for await (const event of this.options.backend.stream(body, {
        signal: abort.signal,
        maxTokens: Math.min(lease.maxTokensPerRequest, descriptor.maxTokens),
        timeoutMs: this.requestTimeoutMs,
      })) {
        if (abort.signal.aborted) break;
        const safeEvent = sanitizeEvent(event);
        const used = terminalTokens(safeEvent);
        if (used !== undefined) {
          if (used > lease.maxTokensPerRequest || lease.usedTokens + used > lease.maxTokensPerJob) {
            throw new ModelProxyError("Model token limit exceeded", "limit_exceeded");
          }
          lease.usedTokens += used;
          requestTokens = used;
          outcome = event.type === "error" ? "provider_error" : "completed";
        }
        const line = `${JSON.stringify({ event: safeEvent })}\n`;
        outputBytes += Buffer.byteLength(line);
        if (outputBytes > MAX_STREAM_BYTES) throw new ModelProxyError("Model stream output limit exceeded", "limit_exceeded");
        response.write(line);
      }
      response.end();
    } catch (error) {
      outcome = abort.signal.aborted ? "cancelled" : error instanceof ModelProxyError ? error.code : "provider_error";
      const message = error instanceof ModelProxyError ? error.message : abort.signal.aborted ? "Model request cancelled" : "Model provider request failed";
      if (!response.headersSent) sendError(response, error instanceof ModelProxyError && ["unauthorized", "expired"].includes(error.code) ? 401 : 400, message);
      else if (!response.destroyed) response.end(`${JSON.stringify({ error: message })}\n`);
    } finally {
      clearTimeout(timeout);
      if (lease) {
        lease.active = false;
        delete lease.abort;
        await this.audit(lease, outcome, requestTokens).catch(() => {});
      }
    }
  }

  private async audit(lease: LeaseState, outcome: string, tokens: number): Promise<void> {
    const record = {
      timestamp: this.now().toISOString(),
      jobId: lease.jobId,
      provider: this.options.backend.descriptor.provider,
      model: this.options.backend.descriptor.id,
      tokens,
      outcome,
    };
    await appendFile(this.options.auditPath, `${JSON.stringify(record)}\n`, { encoding: "utf8", mode: 0o600 });
    await chmod(this.options.auditPath, 0o600);
  }
}

export interface PiModelBackendOptions {
  provider: string;
  model: string;
  apiKey: string;
  baseUrl?: string;
}

/** Trusted-host adapter for Pi's provider implementations. */
export class PiModelBackend implements ModelBackend {
  readonly descriptor: ProxyModelDescriptor;
  private readonly model: Model<string>;

  constructor(private readonly options: PiModelBackendOptions) {
    const found = getModel(options.provider as Parameters<typeof getModel>[0], options.model as Parameters<typeof getModel>[1]);
    if (!found) throw new Error(`Unknown configured model ${options.provider}/${options.model}`);
    this.model = { ...found, ...(options.baseUrl ? { baseUrl: options.baseUrl } : {}) } as Model<string>;
    this.descriptor = {
      provider: found.provider,
      id: found.id,
      name: found.name,
      reasoning: found.reasoning,
      input: [...found.input],
      cost: { ...found.cost },
      contextWindow: found.contextWindow,
      maxTokens: found.maxTokens,
      ...(found.thinkingLevelMap ? { thinkingLevelMap: { ...found.thinkingLevelMap } } : {}),
    };
  }

  stream(request: ModelStreamRequest, options: { signal: AbortSignal; maxTokens: number; timeoutMs: number }): AsyncIterable<AssistantMessageEvent> {
    return streamSimple(this.model, request.context, {
      ...request.options,
      apiKey: this.options.apiKey,
      signal: options.signal,
      maxTokens: options.maxTokens,
      timeoutMs: options.timeoutMs,
      maxRetries: 0,
    });
  }
}

function parseRequest(text: string): ModelStreamRequest {
  let value: unknown;
  try { value = JSON.parse(text); } catch { throw new ModelProxyError("Malformed model request", "invalid_request"); }
  if (!isRecord(value) || !hasOnlyKeys(value, ["provider", "model", "context", "options"]) ||
      typeof value.provider !== "string" || typeof value.model !== "string" || !isRecord(value.context)) {
    throw new ModelProxyError("Malformed model request", "invalid_request");
  }
  if (!Array.isArray(value.context.messages)) throw new ModelProxyError("Model context messages are required", "invalid_request");
  if (value.options !== undefined) {
    if (!isRecord(value.options) || !hasOnlyKeys(value.options, ["reasoning", "temperature", "toolChoice"])) {
      throw new ModelProxyError("Unsupported model request options", "invalid_request");
    }
    if (value.options.reasoning !== undefined && !["minimal", "low", "medium", "high", "xhigh", "max"].includes(String(value.options.reasoning))) {
      throw new ModelProxyError("Unsupported reasoning level", "invalid_request");
    }
    if (value.options.temperature !== undefined && (typeof value.options.temperature !== "number" || !Number.isFinite(value.options.temperature))) {
      throw new ModelProxyError("Invalid temperature", "invalid_request");
    }
    if (value.options.toolChoice !== undefined && !["auto", "none"].includes(String(value.options.toolChoice))) {
      throw new ModelProxyError("Unsupported tool choice", "invalid_request");
    }
  }
  assertJson(value.context);
  if (value.options !== undefined) assertJson(value.options);
  return value as unknown as ModelStreamRequest;
}

function sanitizeEvent(event: AssistantMessageEvent): AssistantMessageEvent {
  const clean = <T extends { diagnostics?: unknown }>(message: T): T => {
    const { diagnostics: _diagnostics, ...rest } = message;
    return rest as T;
  };
  if (event.type === "error") {
    return { ...event, error: { ...clean(event.error), errorMessage: "Model provider request failed" } };
  }
  if (event.type === "done") return { ...event, message: clean(event.message) };
  return { ...event, partial: clean(event.partial) };
}

function terminalTokens(event: AssistantMessageEvent): number | undefined {
  if (event.type !== "done" && event.type !== "error") return undefined;
  const total = event.type === "done" ? event.message.usage.totalTokens : event.error.usage.totalTokens;
  return Number.isSafeInteger(total) && total >= 0 ? total : 0;
}

function hashCredential(credential: string): string { return createHash("sha256").update(credential).digest("hex"); }
function isRecord(value: unknown): value is Record<string, unknown> { return typeof value === "object" && value !== null && !Array.isArray(value); }
function hasOnlyKeys(value: Record<string, unknown>, keys: readonly string[]): boolean { return Object.keys(value).every((key) => keys.includes(key)); }
function singleHeader(value: string | string[] | undefined): string {
  if (typeof value !== "string" || !value) throw new ModelProxyError("Missing model job identity", "unauthorized");
  return value;
}
function parseBearer(value: string | undefined): string {
  if (!value?.startsWith("Bearer ") || value.length > 1024) throw new ModelProxyError("Missing model credential", "unauthorized");
  return value.slice(7);
}
async function readBody(request: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  let bytes = 0;
  for await (const chunk of request) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    bytes += buffer.length;
    if (bytes > MAX_REQUEST_BYTES) throw new ModelProxyError("Model request is too large", "limit_exceeded");
    chunks.push(buffer);
  }
  return Buffer.concat(chunks).toString("utf8");
}
function assertJson(value: unknown, depth = 0): void {
  if (depth > 64) throw new ModelProxyError("Model request is too deeply nested", "invalid_request");
  if (value === null || typeof value === "string" || typeof value === "boolean") return;
  if (typeof value === "number" && Number.isFinite(value)) return;
  if (Array.isArray(value)) { for (const item of value) assertJson(item, depth + 1); return; }
  if (isRecord(value)) { for (const item of Object.values(value)) assertJson(item, depth + 1); return; }
  throw new ModelProxyError("Model request must contain JSON data", "invalid_request");
}
function sendError(response: ServerResponse, status: number, message: string): void {
  const body = JSON.stringify({ error: message });
  response.writeHead(status, { "content-type": "application/json", "content-length": Buffer.byteLength(body), "cache-control": "no-store" });
  response.end(body);
}
