export type ConversationId = string;

export interface IncomingMessage {
  id: string;
  conversationId: ConversationId;
  senderId: string;
  text: string;
  receivedAt: Date;
}

export interface Messenger {
  start(onMessage: (message: IncomingMessage) => Promise<void>): Promise<void>;
  send(conversationId: ConversationId, text: string): Promise<void>;
  close(): Promise<void>;
}

export type ApprovalKind = "question" | "agent-tool" | "mcp-tool";

export interface ApprovalRequest {
  kind: ApprovalKind;
  scopeId: string;
  title: string;
  detail: string;
  choices?: readonly string[];
}

export interface ApprovalPort {
  request(conversationId: ConversationId, request: ApprovalRequest): Promise<string>;
  answer(conversationId: ConversationId, requestId: string, answer: string): boolean;
  cancelScope(conversationId: ConversationId, scopeId: string): void;
}

export interface AgentEvents {
  status(message: string): Promise<void>;
}

export interface AgentRun {
  readonly id: string;
  readonly isRunning: boolean;
  start(prompt: string): Promise<string>;
  steer(message: string): Promise<void>;
  cancel(): Promise<void>;
  dispose(): Promise<void>;
}

export interface AgentFactory {
  create(options: {
    id: string;
    cwd: string;
    conversationId: ConversationId;
    events: AgentEvents;
  }): Promise<AgentRun>;
}
