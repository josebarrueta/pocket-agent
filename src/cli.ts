#!/usr/bin/env node
import { join, resolve } from "node:path";
import { MessageApprovalBroker } from "./approvals.js";
import { loadConfig } from "./config.js";
import { Controller } from "./controller.js";
import { PiSandboxRunner } from "./pi-agent.js";
import { SignalMessenger } from "./signal.js";
import { DisposableWorkspaceManager } from "./workspace.js";

async function main(): Promise<void> {
  const configPath = resolve(process.argv[2] ?? process.env.POCKET_AGENT_CONFIG ?? "config.json");
  const config = await loadConfig(configPath);
  const messenger = new SignalMessenger(
    config.signal.daemonUrl,
    config.signal.account,
    config.signal.allowedSenders,
  );
  const approvals = new MessageApprovalBroker(messenger);
  const sandboxes = new PiSandboxRunner(config, approvals);
  const workspaces = new DisposableWorkspaceManager(join(config.stateDir, "workspaces"), config.repositories);
  await workspaces.reclaimStale(new Date());
  const controller = new Controller(messenger, approvals, sandboxes, workspaces);

  let shuttingDown = false;
  const shutdown = async () => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log("Shutting down...");
    await controller.close();
    await messenger.close();
  };
  process.once("SIGINT", () => void shutdown());
  process.once("SIGTERM", () => void shutdown());

  await messenger.start((message) => controller.handle(message));
  console.log(`Pocket Agent is connected to Signal as ${config.signal.account}`);
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.stack : error);
  process.exitCode = 1;
});
