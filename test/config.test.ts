import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { loadConfig } from "../src/config.js";

const base = {
  signal: { account: "+15550000000", allowedSenders: ["+15550000001"] },
  repositories: { app: "/work/app" },
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

test("sandbox defaults preserve the transitional in-process runner", async () => {
  await withConfig(base, async (path) => {
    const config = await loadConfig(path);
    assert.equal(config.sandbox.runner, "in-process");
    assert.equal(config.sandbox.pids, 256);
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
