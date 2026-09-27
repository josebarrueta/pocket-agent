#!/usr/bin/env node
import { resolve } from "node:path";
import { MessageApprovalBroker } from "./approvals.js";
import { loadConfig } from "./config.js";
import { Controller } from "./controller.js";
import { PiAgentFactory } from "./pi-agent.js";
import { SignalMessenger } from "./signal.js";

async function main(): Promise<void> {
  const configPath = resolve(process.argv[2] ?? process.env.POCKET_AGENT_CONFIG ?? "config.json");
  const config = await loadConfig(configPath);
  const messenger = new SignalMessenger(
    config.signal.daemonUrl,
    config.signal.account,
    config.signal.allowedSenders,
  );
  const approvals = new MessageApprovalBroker(messenger);
  const agents = new PiAgentFactory(config, approvals);
  const controller = new Controller(messenger, approvals, agents, config.repositories);

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
