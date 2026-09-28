import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { loadConfig } from "../src/config.js";

const pinnedImage = `worker@sha256:${"a".repeat(64)}`;
const base = {
  signal: { account: "+15550000000", allowedSenders: ["+15550000001"] },
  repositories: { app: "/work/app" },
  sandbox: { runner: "docker", dockerPath: "/usr/bin/docker", image: pinnedImage },
  agent: { model: "anthropic/claude-sonnet-4-5", apiKeyEnv: "ANTHROPIC_API_KEY" },
};

async function withConfig(value: unknown, run: (path: string) => Promise<void>): Promise<void> {
  const root = await mkdtemp(join(tmpdir(), "pocket-config-"));
  try {
    const path = join(root, "config.json");
    await writeFile(path, JSON.stringify(value));
    await run(path);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
}

test("production configuration requires the Docker sandbox", async () => {
  await withConfig(base, async (path) => {
    const config = await loadConfig(path);
    assert.equal(config.sandbox.runner, "docker");
    assert.equal(config.sandbox.pids, 256);
  });
  const { sandbox: _sandbox, ...withoutSandbox } = base;
  await withConfig(withoutSandbox, async (path) => {
    await assert.rejects(loadConfig(path), /sandbox/);
  });
});

test("model proxy configuration requires a fixed model and credential variable", async () => {
  await withConfig({ ...base, agent: { model: "invalid", apiKeyEnv: "secret" } }, async (path) => {
    await assert.rejects(loadConfig(path), /model|apiKeyEnv/);
  });
  await withConfig(base, async (path) => {
    const config = await loadConfig(path);
    assert.equal(config.agent.modelMaxRequestsPerMinute, 10);
    assert.equal(config.agent.modelMaxTokensPerJob, 200_000);
  });
});

test("Docker sandbox configuration requires absolute executable and pinned image", async () => {
  await withConfig({ ...base, sandbox: { runner: "docker", dockerPath: "docker", image: "worker:latest" } }, async (path) => {
    await assert.rejects(loadConfig(path), /dockerPath must be absolute/);
  });
  await withConfig({ ...base, sandbox: { runner: "docker", dockerPath: "/usr/bin/docker", image: "worker:latest" } }, async (path) => {
    await assert.rejects(loadConfig(path), /pinned by a complete sha256/);
  });
  await withConfig({ ...base, sandbox: { runner: "docker", dockerPath: "/usr/bin/docker", image: `worker@sha256:${"a".repeat(64)}` } }, async (path) => {
    assert.equal((await loadConfig(path)).sandbox.runner, "docker");
  });
});
