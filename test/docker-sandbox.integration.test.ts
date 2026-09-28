import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { CapabilityBroker, type CapabilityLeaseRequest } from "../src/capability-broker.js";
import { DockerSandboxRunner } from "../src/docker-sandbox.js";
import { SandboxFailure } from "../src/sandbox.js";
import type { ApprovalPort } from "../src/types.js";

const execFileAsync = promisify(execFile);
const image = process.env.POCKET_AGENT_DOCKER_TEST_IMAGE;
const fixtureImage = process.env.POCKET_AGENT_DOCKER_FIXTURE_IMAGE;
const dockerPath = process.env.POCKET_AGENT_DOCKER_PATH ?? "/usr/local/bin/docker";
const integration = image ? test : test.skip;

async function docker(...args: string[]): Promise<string> {
  const { stdout } = await execFileAsync(dockerPath, args, { encoding: "utf8", maxBuffer: 4 * 1024 * 1024 });
  return stdout.trim();
}

integration("Docker adapter applies hard isolation settings and cleans up after worker failure", async (t) => {
  const root = await mkdtemp(join(tmpdir(), "pocket-docker-sandbox-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await writeFile(join(root, "input.txt"), "assigned content\n");

  const runner = new DockerSandboxRunner({ dockerPath, image: image!, allowUnpinnedImageForTests: true });
  const job = await runner.create({
    id: `integration-${process.pid}`,
    workspacePath: root,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  t.after(() => job.dispose());

  const containerId = await docker("ps", "--all", "--quiet", "--filter", `label=pocket-agent.job-id=${job.id}`);
  assert.ok(containerId);
  const inspected = JSON.parse(await docker("inspect", containerId))[0];
  assert.equal(inspected.Config.User, "65532:65532");
  assert.equal(inspected.HostConfig.ReadonlyRootfs, true);
  assert.equal(inspected.HostConfig.NetworkMode, "none");
  assert.deepEqual(inspected.HostConfig.CapDrop, ["ALL"]);
  assert.ok(inspected.HostConfig.SecurityOpt.includes("no-new-privileges=true"));
  assert.equal(inspected.HostConfig.PidsLimit, 256);
  assert.equal(inspected.HostConfig.Memory, 1024 * 1024 * 1024);
  assert.equal(inspected.HostConfig.NanoCpus, 1_000_000_000);
  assert.equal(inspected.HostConfig.LogConfig.Type, "none");
  assert.equal(Object.keys(inspected.NetworkSettings.Ports ?? {}).length, 0);
  assert.match(inspected.HostConfig.Tmpfs["/tmp"], /size=268435456/);
  assert.equal(inspected.Mounts.length, 1);
  assert.equal(inspected.Mounts[0].Destination, "/workspace");
  assert.equal(inspected.Mounts[0].Type, "volume");
  const inspectedVolume = JSON.parse(await docker("volume", "inspect", inspected.Mounts[0].Name))[0];
  assert.equal(inspectedVolume.Options.type, "tmpfs");
  assert.match(inspectedVolume.Options.o, /size=805306368/);

  await assert.rejects(job.start("do nothing"), (error: unknown) =>
    error instanceof SandboxFailure && error.code === "internal_error",
  );
  assert.equal(await readFile(join(root, "input.txt"), "utf8"), "assigned content\n");

  const volume = inspected.Mounts[0].Name as string;
  await job.dispose();
  await assert.rejects(docker("inspect", containerId));
  await assert.rejects(docker("volume", "inspect", volume));
});

integration("Docker adapter exports workspace changes and preserves turns", async (t) => {
  if (!fixtureImage) return t.skip("fixture image is not configured");
  const root = await mkdtemp(join(tmpdir(), "pocket-docker-export-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  await writeFile(join(root, "input.txt"), "baseline\n");
  const runner = new DockerSandboxRunner({ dockerPath, image: fixtureImage, allowUnpinnedImageForTests: true });
  const statuses: string[] = [];
  const approvals: string[] = [];
  const job = await runner.create({
    id: `export-${process.pid}`,
    workspacePath: root,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: {
      status: async (message) => { statuses.push(message); },
      approval: async (request) => { approvals.push(request.title); return "yes"; },
    },
  });
  t.after(() => job.dispose());

  assert.equal(await job.start("first"), "completed first");
  assert.equal(await readFile(join(root, "worker.txt"), "utf8"), "first\n");
  assert.equal(await job.start("second"), "completed second");
  assert.equal(await readFile(join(root, "worker.txt"), "utf8"), "first\nsecond\n");
  const steered = job.start("hang");
  await new Promise((resolve) => setTimeout(resolve, 50));
  await job.steer("continue");
  assert.equal(await steered, "completed after steer");
  assert.deepEqual(statuses, ["steered"]);
  assert.equal(await job.start("approval"), "approval handled");
  assert.deepEqual(approvals, ["Allow test tool?"]);
  assert.equal(await readFile(join(root, "approval.txt"), "utf8"), "yes\n");
});

integration("job worker reaches only its authenticated capability scope over private transport", async (t) => {
  if (!fixtureImage) return t.skip("fixture image is not configured");
  if (process.platform === "darwin") return t.skip("Docker Desktop cannot forward host Unix sockets through its VM");
  const root = await mkdtemp(join(tmpdir(), "pocket-docker-broker-"));
  const workspace = join(root, "workspace");
  await mkdir(workspace);
  const approvals: ApprovalPort = { request: async () => "no", answer: () => false, cancelScope: () => {} };
  const broker = new CapabilityBroker({
    socketPath: join(root, "transport", "mcp.sock"),
    auditPath: join(root, "audit.ndjson"),
    approvals,
    tools: [{
      name: "workspace.read_metadata",
      description: "Read metadata",
      inputSchema: { type: "object" },
      policy: "allow",
      normalize: () => ({}),
      invoke: async () => ({}),
    }],
  });
  await broker.start();
  let issuedCredential = "";
  const leases = {
    issue(request: CapabilityLeaseRequest) {
      const granted = broker.issue(request);
      issuedCredential = granted.credential;
      return granted;
    },
  };
  t.after(async () => { await broker.close(); await rm(root, { recursive: true, force: true }); });
  const runner = new DockerSandboxRunner({
    dockerPath,
    image: fixtureImage,
    allowUnpinnedImageForTests: true,
    capabilityLeases: leases,
    allowedCapabilities: ["workspace.read_metadata"],
  });
  const job = await runner.create({
    id: `broker-${process.pid}`,
    workspacePath: workspace,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  const containerId = await docker("ps", "--all", "--quiet", "--filter", `label=pocket-agent.job-id=${job.id}`);
  const inspected = JSON.parse(await docker("inspect", containerId))[0];
  assert.equal(inspected.HostConfig.NetworkMode, "none");
  const brokerMount = inspected.Mounts.find((mount: { Destination: string }) => mount.Destination === "/run/pocket-agent-broker");
  assert.equal(brokerMount.Type, "bind");
  assert.equal(brokerMount.RW, false);

  assert.equal(await job.start("broker-list"), "completed broker-list");
  const response = JSON.parse(await readFile(join(workspace, "broker.json"), "utf8"));
  assert.deepEqual(response.result.tools.map((tool: { name: string }) => tool.name), ["workspace.read_metadata"]);
  await job.dispose();
  await assert.rejects(broker.list(issuedCredential, job.id), /revoked/);
});

integration("sandboxed test tool cannot read host files, environment secrets, or Docker socket", async (t) => {
  if (!fixtureImage) return t.skip("fixture image is not configured");
  const workspace = await mkdtemp(join(tmpdir(), "pocket-docker-isolation-"));
  const hostSecretPath = join(homedir(), `.pocket-agent-host-secret-${process.pid}-${Date.now()}`);
  await writeFile(hostSecretPath, "host-only\n", { mode: 0o600 });
  t.after(() => Promise.all([
    rm(workspace, { recursive: true, force: true }),
    rm(hostSecretPath, { force: true }),
  ]));
  const previousSecret = process.env.POCKET_AGENT_HOST_TEST_SECRET;
  process.env.POCKET_AGENT_HOST_TEST_SECRET = "must-not-enter-worker";
  t.after(() => {
    if (previousSecret === undefined) delete process.env.POCKET_AGENT_HOST_TEST_SECRET;
    else process.env.POCKET_AGENT_HOST_TEST_SECRET = previousSecret;
  });

  const runner = new DockerSandboxRunner({ dockerPath, image: fixtureImage, allowUnpinnedImageForTests: true });
  const job = await runner.create({
    id: `isolation-${process.pid}`,
    workspacePath: workspace,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  t.after(() => job.dispose());
  await job.start(`probe-isolation:${hostSecretPath}`);
  assert.deepEqual(JSON.parse(await readFile(join(workspace, "isolation.json"), "utf8")), {
    inheritedSecret: null,
    hostFileAccessible: false,
    dockerSocketAccessible: false,
  });
});

integration("Docker adapter bounds deadlines and reports worker crashes", async (t) => {
  if (!fixtureImage) return t.skip("fixture image is not configured");
  const root = await mkdtemp(join(tmpdir(), "pocket-docker-limits-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const runner = new DockerSandboxRunner({ dockerPath, image: fixtureImage, allowUnpinnedImageForTests: true });

  const crashing = await runner.create({
    id: `crash-${process.pid}`,
    workspacePath: root,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  await assert.rejects(crashing.start("crash"), (error: unknown) =>
    error instanceof SandboxFailure && error.code === "worker_crash",
  );
  await crashing.dispose();

  const hanging = await runner.create({
    id: `timeout-${process.pid}`,
    workspacePath: root,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 1_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  await assert.rejects(hanging.start("hang"), (error: unknown) =>
    error instanceof SandboxFailure && error.code === "deadline_exceeded",
  );
  await hanging.dispose();

  const cancelled = await runner.create({
    id: `cancel-${process.pid}`,
    workspacePath: root,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  const cancellation = assert.rejects(cancelled.start("hang"), /cancelled/);
  await new Promise((resolve) => setTimeout(resolve, 100));
  await cancelled.cancel();
  await cancellation;
  assert.equal(await docker("ps", "--all", "--quiet", "--filter", `label=pocket-agent.job-id=${cancelled.id}`), "");
});

integration("Docker kernel limits contain process and memory pressure", async (t) => {
  if (!fixtureImage) return t.skip("fixture image is not configured");
  const root = await mkdtemp(join(tmpdir(), "pocket-docker-pressure-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const runner = new DockerSandboxRunner({
    dockerPath,
    image: fixtureImage,
    allowUnpinnedImageForTests: true,
    limits: { pids: 32, memoryBytes: 134_217_728 },
  });

  const forked = await runner.create({
    id: `fork-${process.pid}`,
    workspacePath: root,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  await assert.rejects(forked.start("fork-pressure"), (error: unknown) =>
    error instanceof SandboxFailure && error.code === "worker_crash",
  );
  await forked.dispose();

  const pressured = await runner.create({
    id: `memory-${process.pid}`,
    workspacePath: root,
    conversationId: "test",
    repositoryScope: "repo",
    deadlineAt: new Date(Date.now() + 30_000),
    outputLimitBytes: 64 * 1024,
    events: { status: async () => {} },
  });
  await assert.rejects(pressured.start("memory-pressure"), (error: unknown) =>
    error instanceof SandboxFailure && error.code === "worker_crash",
  );
  await pressured.dispose();
});

integration("Docker reconciliation removes labeled orphan containers and volumes", async () => {
  const runner = new DockerSandboxRunner({ dockerPath, image: image!, allowUnpinnedImageForTests: true });
  const suffix = `${process.pid}-${Date.now()}`;
  const volume = `pocket-agent-integration-orphan-${suffix}`;
  const container = `pocket-agent-integration-orphan-${suffix}`;
  await docker("volume", "create", "--label", "pocket-agent.managed=true", volume);
  await docker(
    "create", "--name", container,
    "--label", "pocket-agent.managed=true",
    "--mount", `type=volume,src=${volume},dst=/workspace`,
    image!, "--smoke-test",
  );

  await runner.reconcile();
  await assert.rejects(docker("inspect", container));
  await assert.rejects(docker("volume", "inspect", volume));
});
