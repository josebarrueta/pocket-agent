import { Client } from "@modelcontextprotocol/client";
import { StdioClientTransport } from "@modelcontextprotocol/client/stdio";
import type { InlineExtension } from "@earendil-works/pi-coding-agent";
import { Type } from "typebox";
import type { McpServerConfig, ToolDecision } from "./config.js";
import type { ApprovalPort, ConversationId } from "./types.js";

interface Connection {
  client: Client;
  transport: StdioClientTransport;
}

function safeName(value: string): string {
  return value.replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 64);
}

function renderMcpContent(content: unknown): string {
  if (!Array.isArray(content)) return JSON.stringify(content);
  return content.map((item: unknown) => {
    if (item && typeof item === "object" && "text" in item && typeof item.text === "string") {
      return item.text;
    }
    return JSON.stringify(item);
  }).join("\n");
}

/**
 * Connects operator-configured stdio MCP servers and turns approved tools into Pi tools.
 * Configuration is never accepted over chat. Stdio is shell-free, but the configured
 * process still has the daemon's OS privileges; use an OS/container sandbox for untrusted servers.
 */
export async function createMcpExtension(options: {
  servers: Record<string, McpServerConfig>;
  approvals: ApprovalPort;
  conversationId: ConversationId;
  scopeId: string;
}): Promise<{ extension: InlineExtension; close: () => Promise<void> }> {
  const connections: Connection[] = [];
  const registrations: Array<{
    registeredName: string;
    serverName: string;
    toolName: string;
    description?: string;
    inputSchema: Record<string, unknown>;
    decision: ToolDecision;
    timeoutMs: number;
    client: Client;
  }> = [];

  try {
    for (const [serverName, config] of Object.entries(options.servers)) {
      const client = new Client({ name: "pocket-agent", version: "0.1.0" });
      const transport = new StdioClientTransport({
        command: config.command,
        args: config.args,
        ...(config.cwd ? { cwd: config.cwd } : {}),
        env: config.env,
        stderr: "pipe",
        maxBufferSize: 2 * 1024 * 1024,
      });
      await client.connect(transport);
      connections.push({ client, transport });

      let cursor: string | undefined;
      do {
        const page = await client.listTools(cursor ? { cursor } : undefined);
        for (const tool of page.tools) {
          const decision = config.tools[tool.name] ?? config.defaultToolDecision;
          if (decision === "deny") continue;
          registrations.push({
            registeredName: `mcp_${safeName(serverName)}_${safeName(tool.name)}`,
            serverName,
            toolName: tool.name,
            ...(tool.description ? { description: tool.description } : {}),
            inputSchema: tool.inputSchema as Record<string, unknown>,
            decision,
            timeoutMs: config.timeoutMs,
            client,
          });
        }
        cursor = page.nextCursor;
      } while (cursor);
    }
  } catch (error) {
    await Promise.allSettled(connections.map(({ client }) => client.close()));
    throw error;
  }

  const extension: InlineExtension = (pi) => {
    for (const registration of registrations) {
      pi.registerTool({
        name: registration.registeredName,
        label: `${registration.serverName}: ${registration.toolName}`,
        description: registration.description ?? `MCP tool ${registration.toolName}`,
        parameters: Type.Unsafe(registration.inputSchema),
        async execute(_id, params, signal) {
          if (registration.decision === "ask") {
            const answer = await options.approvals.request(options.conversationId, {
              kind: "mcp-tool",
              scopeId: options.scopeId,
              title: `Allow MCP tool ${registration.serverName}/${registration.toolName}?`,
              detail: JSON.stringify(params, null, 2).slice(0, 8_000),
              choices: ["yes", "no"],
            });
            if (answer !== "yes") throw new Error("MCP tool denied by operator");
          }
          if (signal?.aborted) throw new Error("MCP tool cancelled");
          const result = await registration.client.callTool(
            { name: registration.toolName, arguments: params as Record<string, unknown> },
            { timeout: registration.timeoutMs, maxTotalTimeout: registration.timeoutMs },
          );
          const text = renderMcpContent(result.content).slice(0, 50_000);
          if (result.isError) throw new Error(text || "MCP tool returned an error");
          return { content: [{ type: "text", text }], details: { server: registration.serverName } };
        },
      });
    }
  };

  return {
    extension,
    close: async () => {
      await Promise.allSettled(connections.map(({ client }) => client.close()));
    },
  };
}
