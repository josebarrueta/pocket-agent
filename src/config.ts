import { readFile } from "node:fs/promises";
import { isAbsolute, resolve } from "node:path";
import { z } from "zod";

const toolDecision = z.enum(["allow", "ask", "deny"]);

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
  sandbox: z.object({
    runner: z.literal("docker"),
    dockerPath: z.string().default("/usr/local/bin/docker"),
    image: z.string().min(1),
    cpus: z.number().positive().max(64).default(1),
    memoryBytes: z.number().int().positive().default(1_073_741_824),
    pids: z.number().int().positive().max(4096).default(256),
    temporaryStorageBytes: z.number().int().positive().default(268_435_456),
    workspaceStorageBytes: z.number().int().positive().default(805_306_368),
  }),
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
});

export type AppConfig = z.infer<typeof configSchema>;

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
  if (!isAbsolute(config.sandbox.dockerPath)) throw new Error("sandbox.dockerPath must be absolute");
  if (!/@sha256:[a-fA-F0-9]{64}$/.test(config.sandbox.image)) {
    throw new Error("sandbox.image must be pinned by a complete sha256 digest");
  }
  for (const [name, repo] of Object.entries(config.repositories)) {
    config.repositories[name] = expandHome(repo);
  }
  return config;
}
