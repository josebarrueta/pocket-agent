import WebSocket, { type RawData } from "ws";
import type { IncomingMessage, Messenger } from "./types.js";

interface SignalEnvelope {
  source?: string;
  sourceNumber?: string;
  sourceUuid?: string;
  timestamp?: number;
  dataMessage?: {
    message?: string;
    groupInfo?: unknown;
    timestamp?: number;
  };
  syncMessage?: unknown;
}

interface SignalEvent {
  account?: string;
  envelope?: SignalEnvelope;
  method?: string;
  params?: {
    account?: string;
    envelope?: SignalEnvelope;
    result?: { envelope?: SignalEnvelope; account?: string };
  };
}

export class SignalMessenger implements Messenger {
  private readonly allowed: Set<string>;
  private socket: WebSocket | undefined;
  private reconnectTimer: NodeJS.Timeout | undefined;
  private stopped = false;
  private onMessage?: (message: IncomingMessage) => Promise<void>;
  private messageQueue: Promise<void> = Promise.resolve();

  constructor(
    private readonly baseUrl: string,
    private readonly account: string,
    allowedSenders: readonly string[],
  ) {
    this.allowed = new Set(allowedSenders);
  }

  async start(onMessage: (message: IncomingMessage) => Promise<void>): Promise<void> {
    if (this.onMessage) throw new Error("Signal messenger is already started");
    this.onMessage = onMessage;
    await this.waitUntilReachable();
    await this.connect();
  }

  async send(conversationId: string, text: string): Promise<void> {
    const response = await fetch(new URL("/v2/send", this.baseUrl), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        number: this.account,
        recipients: [conversationId],
        message: text,
      }),
      signal: AbortSignal.timeout(15_000),
    });
    if (!response.ok) {
      const detail = await response.text();
      throw new Error(`Signal send failed: HTTP ${response.status} ${detail.slice(0, 500)}`);
    }
  }

  async close(): Promise<void> {
    this.stopped = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    if (this.socket) {
      await new Promise<void>((resolve) => {
        const socket = this.socket!;
        socket.once("close", () => resolve());
        socket.close();
        setTimeout(() => { socket.terminate(); resolve(); }, 1_000).unref();
      });
    }
    await this.messageQueue;
  }

  private async waitUntilReachable(): Promise<void> {
    const response = await fetch(new URL("/v1/health", this.baseUrl), {
      signal: AbortSignal.timeout(5_000),
    });
    if (!response.ok) throw new Error(`signal-cli REST API is not ready: HTTP ${response.status}`);
  }

  private async connect(): Promise<void> {
    const endpoint = new URL(`/v1/receive/${encodeURIComponent(this.account)}`, this.baseUrl);
    endpoint.protocol = endpoint.protocol === "https:" ? "wss:" : "ws:";

    await new Promise<void>((resolve, reject) => {
      const socket = new WebSocket(endpoint);
      this.socket = socket;
      const initialError = (error: Error) => reject(error);
      socket.once("error", initialError);
      socket.once("open", () => {
        socket.off("error", initialError);
        resolve();
      });
      socket.on("message", (data) => this.enqueueMessage(data));
      socket.on("error", (error) => console.error("Signal WebSocket error:", error.message));
      socket.once("close", () => {
        if (this.socket === socket) this.socket = undefined;
        if (!this.stopped) this.scheduleReconnect();
      });
    });
  }

  private scheduleReconnect(): void {
    if (this.reconnectTimer || this.stopped) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = undefined;
      void this.connect().catch((error: unknown) => {
        console.error("Signal WebSocket reconnect failed:", error);
        this.scheduleReconnect();
      });
    }, 2_000);
  }

  private enqueueMessage(data: RawData): void {
    this.messageQueue = this.messageQueue
      .then(async () => {
        const event = JSON.parse(data.toString()) as SignalEvent;
        await this.handleEvent(event);
      })
      .catch((error: unknown) => console.error("Invalid Signal event:", error));
  }

  private async handleEvent(event: SignalEvent): Promise<void> {
    if (event.method && event.method !== "receive") return;
    const envelope = event.envelope ?? event.params?.envelope ?? event.params?.result?.envelope;
    const account = event.account ?? event.params?.account ?? event.params?.result?.account;
    if (!envelope || (account && account !== this.account) || envelope.syncMessage) return;
    if (envelope.dataMessage?.groupInfo) return; // MVP intentionally accepts private chats only.

    const candidates = [envelope.sourceNumber, envelope.sourceUuid, envelope.source].filter(
      (value): value is string => Boolean(value),
    );
    const sender = candidates.find((candidate) => this.allowed.has(candidate));
    const text = envelope.dataMessage?.message?.trim();
    if (!sender || !text || !this.onMessage) return;

    await this.onMessage({
      id: `${envelope.dataMessage?.timestamp ?? envelope.timestamp ?? Date.now()}`,
      conversationId: sender,
      senderId: sender,
      text,
      receivedAt: new Date(envelope.timestamp ?? Date.now()),
    });
  }
}
