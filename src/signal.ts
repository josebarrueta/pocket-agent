import { randomUUID } from "node:crypto";
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

interface SignalNotification {
  method?: string;
  params?: {
    account?: string;
    envelope?: SignalEnvelope;
    result?: { envelope?: SignalEnvelope; account?: string };
  };
}

export class SignalMessenger implements Messenger {
  private readonly abortController = new AbortController();
  private readonly allowed: Set<string>;
  private loop?: Promise<void>;

  constructor(
    private readonly baseUrl: string,
    private readonly account: string,
    allowedSenders: readonly string[],
  ) {
    this.allowed = new Set(allowedSenders);
  }

  async start(onMessage: (message: IncomingMessage) => Promise<void>): Promise<void> {
    if (this.loop) throw new Error("Signal messenger is already started");
    await this.waitUntilReachable();
    this.loop = this.receiveLoop(onMessage);
  }

  async send(conversationId: string, text: string): Promise<void> {
    const response = await fetch(new URL("/api/v1/rpc", this.baseUrl), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: randomUUID(),
        method: "send",
        params: { account: this.account, recipient: [conversationId], message: text },
      }),
      signal: AbortSignal.timeout(15_000),
    });
    if (!response.ok) throw new Error(`Signal send failed: HTTP ${response.status}`);
    const result = await response.json() as { error?: { message?: string } };
    if (result.error) throw new Error(`Signal send failed: ${result.error.message ?? "JSON-RPC error"}`);
  }

  async close(): Promise<void> {
    this.abortController.abort();
    await this.loop?.catch((error: unknown) => {
      if (!this.abortController.signal.aborted) throw error;
    });
  }

  private async waitUntilReachable(): Promise<void> {
    const response = await fetch(new URL("/api/v1/check", this.baseUrl), {
      signal: AbortSignal.timeout(5_000),
    });
    if (!response.ok) throw new Error(`signal-cli daemon is not ready: HTTP ${response.status}`);
  }

  private async receiveLoop(onMessage: (message: IncomingMessage) => Promise<void>): Promise<void> {
    while (!this.abortController.signal.aborted) {
      try {
        await this.consumeEvents(onMessage);
      } catch (error) {
        if (this.abortController.signal.aborted) return;
        console.error("Signal event stream disconnected:", error);
        await new Promise((resolve) => setTimeout(resolve, 2_000));
      }
    }
  }

  private async consumeEvents(onMessage: (message: IncomingMessage) => Promise<void>): Promise<void> {
    const response = await fetch(new URL("/api/v1/events", this.baseUrl), {
      headers: { accept: "text/event-stream" },
      signal: this.abortController.signal,
    });
    if (!response.ok || !response.body) {
      throw new Error(`Signal events failed: HTTP ${response.status}`);
    }

    const reader = response.body.pipeThrough(new TextDecoderStream()).getReader();
    let buffer = "";
    while (!this.abortController.signal.aborted) {
      const { value, done } = await reader.read();
      if (done) throw new Error("event stream ended");
      buffer += value;
      const events = buffer.split(/\r?\n\r?\n/);
      buffer = events.pop() ?? "";
      for (const event of events) {
        const data = event.split(/\r?\n/)
          .filter((line) => line.startsWith("data:"))
          .map((line) => line.slice(5).trimStart())
          .join("\n");
        if (!data) continue;
        await this.handleNotification(JSON.parse(data) as SignalNotification, onMessage);
      }
    }
  }

  private async handleNotification(
    notification: SignalNotification,
    onMessage: (message: IncomingMessage) => Promise<void>,
  ): Promise<void> {
    if (notification.method !== "receive") return;
    const envelope = notification.params?.envelope ?? notification.params?.result?.envelope;
    const account = notification.params?.account ?? notification.params?.result?.account;
    if (!envelope || (account && account !== this.account) || envelope.syncMessage) return;
    if (envelope.dataMessage?.groupInfo) return; // MVP intentionally accepts private chats only.

    const candidates = [envelope.sourceNumber, envelope.sourceUuid, envelope.source].filter(
      (value): value is string => Boolean(value),
    );
    const sender = candidates.find((candidate) => this.allowed.has(candidate));
    const text = envelope.dataMessage?.message?.trim();
    if (!sender || !text) return;

    await onMessage({
      id: `${envelope.dataMessage?.timestamp ?? envelope.timestamp ?? Date.now()}`,
      conversationId: sender,
      senderId: sender,
      text,
      receivedAt: new Date(envelope.timestamp ?? Date.now()),
    });
  }
}
