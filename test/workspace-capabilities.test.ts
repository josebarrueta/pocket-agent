import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { CapabilityBroker } from "../src/capability-broker.js";
import type { ApprovalPort, ApprovalRequest } from "../src/types.js";
import { createWorkspaceCapabilityTools, WORKSPACE_CAPABILITY_NAMES } from "../src/workspace-capabilities.js";
import { DisposableWorkspaceManager } from "../src/workspace.js";

const execFileAsync = promisify(execFile);

class AllowApprovals implements ApprovalPort {
  requests: ApprovalRequest[] = [];
  async request(_conversationId: string, request: ApprovalRequest): Promise<string> {
    this.requests.push(request);
    return "yes";
  }
  answer(): boolean { return false; }
  cancelScope(): void {}
}

async function git(cwd: string, ...args: string[]): Promise<string> {
  const { stdout } = await execFileAsync("git", args, { cwd, encoding: "utf8" });
  return stdout;
}

test("workspace MCP capabilities derive scope from the lease and separate submit from approved apply", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "pocket-workspace-tools-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const source = join(root, "source");
  await mkdir(source);
  await git(source, "init", "--quiet");
  await writeFile(join(source, "file.txt"), "before\n");
  await git(source, "add", ".");
  await git(source, "-c", "user.name=Test", "-c", "user.email=test@invalid", "commit", "--quiet", "-m", "baseline");

  const manager = new DisposableWorkspaceManager(join(root, "storage"), { app: source });
  const workspace = await manager.create("job-a", "app");
  t.after(() => workspace.dispose());
  await writeFile(join(source, "file.txt"), "after\n");
  const patch = await git(source, "diff", "--binary", "HEAD", "--", "file.txt");
  await git(source, "checkout", "--quiet", "--", "file.txt");

  const approvals = new AllowApprovals();
  const broker = new CapabilityBroker({
    socketPath: join(root, "broker", "mcp.sock"),
    auditPath: join(root, "audit.ndjson"),
    tools: createWorkspaceCapabilityTools(manager),
    approvals,
    now: () => new Date("2026-01-01T00:00:00Z"),
  });
  await broker.start();
  t.after(() => broker.close());
  const lease = broker.issue({
    jobId: "job-a",
    conversationId: "operator",
    repositoryScope: "app",
    allowedTools: WORKSPACE_CAPABILITY_NAMES,
    expiresAt: new Date("2026-01-01T01:00:00Z"),
  });

  const metadata = await broker.call(lease.credential, "job-a", "metadata", "workspace.read_metadata", {});
  assert.deepEqual(metadata, {
    jobId: "job-a",
    repositoryAlias: "app",
    patchLimits: { maxBytes: 2 * 1024 * 1024, maxFiles: 200 },
  });
  assert.doesNotMatch(JSON.stringify(metadata), new RegExp(root.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  await assert.rejects(
    broker.call(lease.credential, "job-a", "bad", "workspace.submit_patch", { patch, path: source }),
    /missing or unknown fields/,
  );

  const submitted = await broker.call(lease.credential, "job-a", "submit", "workspace.submit_patch", { patch }) as { patchId: string };
  assert.equal(await readFile(join(source, "file.txt"), "utf8"), "before\n");
  const status = await broker.call(lease.credential, "job-a", "status", "workspace.get_patch_status", {});
  assert.equal((status as { state: string }).state, "submitted");
  assert.match((status as { patch: string }).patch, /\+after/);

  const applied = await broker.call(lease.credential, "job-a", "apply", "workspace.apply_patch", { patchId: submitted.patchId });
  assert.equal((applied as { state: string }).state, "applied");
  assert.equal(await readFile(join(source, "file.txt"), "utf8"), "after\n");
  assert.equal(approvals.requests.length, 1);
  assert.equal(approvals.requests[0]!.operation?.tool, "workspace.apply_patch");
});
