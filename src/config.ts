import { readFile } from "node:fs/promises";
import { isAbsolute, resolve } from "node:path";
import { z } from "zod";

const toolDecision = z.enum(["allow", "ask", "deny"]);

const mcpServerSchema = z.object({
  command: z.string().min(1),
  args: z.array(z.string()).default([]),
  cwd: z.string().optional(),
  env: z.record(z.string(), z.string()).default({}),
  tools: z.record(z.string(), toolDecision).default({}),
  defaultToolDecision: toolDecision.default("deny"),
  timeoutMs: z.number().int().min(100).max(600_000).default(60_000),
});

export const configSchema = z.object({
  signal: z.object({
    daemonUrl: z.string().url().default("http://127.0.0.1:8080"),
    account: z.string().min(1),
    allowedSenders: z.array(z.string().min(1)).min(1),
  }),
  repositories: z.record(z.string(), z.string()).refine(
    (repos) => Object.keys(repos).length > 0,
    "at least one repository is required",
  ),
  stateDir: z.string().default("~/.local/share/pocket-agent"),
  agent: z.object({
    model: z.string().optional(),
    thinking: z.enum(["off", "minimal", "low", "medium", "high", "xhigh", "max"]).default("medium"),
    permissions: z.object({
      read: toolDecision.default("allow"),
      write: toolDecision.default("ask"),
      bash: toolDecision.default("ask"),
    }).default({ read: "allow", write: "ask", bash: "ask" }),
  }).default({
    thinking: "medium",
    permissions: { read: "allow", write: "ask", bash: "ask" },
  }),
  mcpServers: z.record(z.string(), mcpServerSchema).default({}),
});

export type AppConfig = z.infer<typeof configSchema>;
export type McpServerConfig = z.infer<typeof mcpServerSchema>;
export type ToolDecision = z.infer<typeof toolDecision>;

function expandHome(path: string): string {
  if (path === "~" || path.startsWith("~/")) {
    const home = process.env.HOME;
    if (!home) throw new Error("HOME is not set");
    return resolve(home, path.slice(2));
  }
  return resolve(path);
}

export async function loadConfig(path: string): Promise<AppConfig> {
  const parsed: unknown = JSON.parse(await readFile(path, "utf8"));
  const config = configSchema.parse(parsed);

  config.stateDir = expandHome(config.stateDir);
  for (const [name, repo] of Object.entries(config.repositories)) {
    config.repositories[name] = expandHome(repo);
  }
  for (const [name, server] of Object.entries(config.mcpServers)) {
    if (!isAbsolute(server.command)) {
      throw new Error(`mcpServers.${name}.command must be an absolute path (no PATH lookup or shell)`);
    }
    if (server.cwd) server.cwd = expandHome(server.cwd);
  }
  return config;
}
