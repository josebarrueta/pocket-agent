import type { ApprovalPort, ApprovalRequest, ConversationId, Messenger } from "./types.js";

interface PendingApproval {
  conversationId: ConversationId;
  request: ApprovalRequest;
  resolve: (answer: string) => void;
}

export class MessageApprovalBroker implements ApprovalPort {
  private nextId = 1;
  private readonly pending = new Map<string, PendingApproval>();

  constructor(private readonly messenger: Messenger) {}

  async request(conversationId: ConversationId, request: ApprovalRequest): Promise<string> {
    const id = String(this.nextId++);
    const choices = request.choices?.length ? `\nChoices: ${request.choices.join(" | ")}` : "";
    await this.messenger.send(
      conversationId,
      `❓ [${id}] ${request.title}\n${request.detail}${choices}\nReply: /answer ${id} <answer>`,
    );
    return new Promise<string>((resolve) => {
      this.pending.set(id, { conversationId, request, resolve });
    });
  }

  answer(conversationId: ConversationId, requestId: string, answer: string): boolean {
    const pending = this.pending.get(requestId);
    if (!pending || pending.conversationId !== conversationId) return false;
    if (pending.request.choices?.length) {
      const normalized = answer.trim().toLowerCase();
      const choice = pending.request.choices.find((item) => item.toLowerCase() === normalized);
      if (!choice) return false;
      answer = choice;
    }
    this.pending.delete(requestId);
    pending.resolve(answer);
    return true;
  }

  cancelScope(conversationId: ConversationId, scopeId: string): void {
    for (const [id, pending] of this.pending) {
      if (pending.conversationId !== conversationId || pending.request.scopeId !== scopeId) continue;
      this.pending.delete(id);
      pending.resolve("cancel");
    }
  }
}
