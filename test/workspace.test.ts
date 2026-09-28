import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, mkdir, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { DisposableWorkspaceManager, type WorkspaceCapabilityTarget } from "../src/workspace.js";

const execFileAsync = promisify(execFile);

async function git(cwd: string, ...args: string[]): Promise<string> {
  const { stdout } = await execFileAsync("git", args, {
    cwd,
    encoding: "utf8",
    env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null" },
  });
  return stdout.trim();
}

async function repository(root: string, name: string): Promise<string> {
  const path = join(root, name);
  await mkdir(path);
  await git(path, "init", "--quiet");
  await writeFile(join(path, "kept.txt"), "baseline\n");
  await writeFile(join(path, "delete.txt"), "remove me\n");
  await git(path, "add", ".");
  await git(path, "-c", "user.name=Test", "-c", "user.email=test@invalid", "commit", "--quiet", "-m", "baseline");
  return path;
}

async function candidatePatch(source: string, mutate: () => Promise<void>): Promise<string> {
  await mutate();
  await git(source, "add", "--all");
  const { stdout } = await execFileAsync("git", ["diff", "--cached", "--binary", "--full-index", "HEAD"], {
    cwd: source,
    encoding: "utf8",
  });
  await git(source, "reset", "--hard", "--quiet", "HEAD");
  await git(source, "clean", "-ffdqx");
  return stdout;
}

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "pocket-workspace-"));
  const source = await repository(root, "source");
  const storage = join(root, "storage");
  const manager = new DisposableWorkspaceManager(storage, { app: source });
  return { root, source, storage, manager };
}

test("creates a disposable snapshot from an exact configured alias", async (t) => {
  const { root, source, manager } = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));
  await writeFile(join(source, "untracked.txt"), "included\n");
  await writeFile(join(source, ".ignored"), "secret\n");
  await writeFile(join(source, ".gitignore"), ".ignored\n");

  const workspace = await manager.create("job-1", "app");
  assert.notEqual(workspace.path, source);
  assert.equal(await readFile(join(workspace.path, "kept.txt"), "utf8"), "baseline\n");
  assert.equal(await readFile(join(workspace.path, "untracked.txt"), "utf8"), "included\n");
  await assert.rejects(readFile(join(workspace.path, ".ignored")), /ENOENT/);
  await assert.rejects(readFile(join(workspace.path, ".git")), /ENOENT|EISDIR/);

  await writeFile(join(workspace.path, "kept.txt"), "worker edit\n");
  assert.equal(await readFile(join(source, "kept.txt"), "utf8"), "baseline\n");
  await workspace.dispose();
});

test("does not let a worker select an unconfigured repository or path", async (t) => {
  const { root, manager } = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));
  const secret = await repository(root, "secret-repository");
  await writeFile(join(secret, "private.txt"), "do not copy\n");

  await assert.rejects(manager.create("job-2", "../secret-repository"), /Unknown repository alias/);
  await assert.rejects(manager.create("../job-2", "app"), /Invalid workspace job identity/);
  assert.deepEqual(manager.aliases, ["app"]);
});

test("exports a bounded binary-capable patch and changed-file manifest", async (t) => {
  const { root, manager } = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));
  const workspace = await manager.create("job-3", "app");

  await writeFile(join(workspace.path, "kept.txt"), "changed\n");
  await rm(join(workspace.path, "delete.txt"));
  await mkdir(join(workspace.path, "new"));
  await writeFile(join(workspace.path, "new", "added.txt"), "added\n");
  await writeFile(join(workspace.path, "binary.bin"), Buffer.from([0, 1, 2, 255]));

  const result = await workspace.exportPatch();
  assert.deepEqual(result.files, [
    { path: "binary.bin", status: "added" },
    { path: "delete.txt", status: "deleted" },
    { path: "kept.txt", status: "modified" },
    { path: "new/added.txt", status: "added" },
  ]);
  assert.match(result.patch, /diff --git a\/kept\.txt b\/kept\.txt/);
  assert.match(result.patch, /GIT binary patch/);
  assert.doesNotMatch(result.patch, new RegExp(root.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  await workspace.dispose();
});

test("rejects escaping links, submodules, oversized patches, and excess changed files", async (t) => {
  const { root, source, manager } = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));

  await symlink(join(root, "outside"), join(source, "escape"));
  await assert.rejects(manager.create("symlink-source", "app"), /Symbolic links/);
  await rm(join(source, "escape"));

  const head = await git(source, "rev-parse", "HEAD");
  await git(source, "update-index", "--add", "--cacheinfo", `160000,${head},vendor`);
  await assert.rejects(manager.create("submodule-source", "app"), /submodules/);
  await git(source, "update-index", "--force-remove", "vendor");

  const workspace = await manager.create("bounded", "app");
  await writeFile(join(workspace.path, "one.txt"), "1\n");
  await writeFile(join(workspace.path, "two.txt"), "2\n");
  await assert.rejects(workspace.exportPatch({ maxFiles: 1 }), /changes 2 files/);
  await assert.rejects(workspace.exportPatch({ maxBytes: 20 }), /Patch exceeds 20 bytes/);

  await symlink(join(root, "outside"), join(workspace.path, "escape"));
  await assert.rejects(workspace.exportPatch(), /Symbolic links/);
  await workspace.dispose();
});

test("submits, reviews, and applies a validated patch only to its configured repository", async (t) => {
  const { root, source, manager } = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));
  const workspace = await manager.create("capability", "app");
  const target = manager.resolve("capability", "app");
  assert.ok(target);
  assert.equal(manager.resolve("capability", "other"), undefined);
  const patch = await candidatePatch(source, async () => {
    await writeFile(join(source, "kept.txt"), "approved change\n");
    await rm(join(source, "delete.txt"));
    await writeFile(join(source, "added.txt"), "added\n");
  });

  const submitted = await target.submitPatch(patch);
  assert.equal(submitted.state, "submitted");
  assert.deepEqual(submitted.files, [
    { path: "added.txt", status: "added" },
    { path: "delete.txt", status: "deleted" },
    { path: "kept.txt", status: "modified" },
  ]);
  assert.match(submitted.patchId!, /^[a-f0-9]{64}$/);
  await assert.rejects(target.applyPatch("0".repeat(64)), /stale patch ID/);
  const applied = await target.applyPatch(submitted.patchId!);
  assert.equal(applied.state, "applied");
  assert.equal(await readFile(join(source, "kept.txt"), "utf8"), "approved change\n");
  assert.equal(await readFile(join(source, "added.txt"), "utf8"), "added\n");
  await assert.rejects(readFile(join(source, "delete.txt")), /ENOENT/);
  await workspace.dispose();
  assert.equal(manager.resolve("capability", "app"), undefined);
});

test("broker patch validation rejects binary, traversal, symlink, rename, submodule, and oversized input", async (t) => {
  const { root, source } = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));
  const manager = new DisposableWorkspaceManager(join(root, "bounded-storage"), { app: source }, {
    patchLimits: { maxBytes: 2_000, maxFiles: 5 },
  });
  await manager.create("adversarial", "app");
  const target = manager.resolve("adversarial", "app") as WorkspaceCapabilityTarget;

  const binary = await candidatePatch(source, async () => writeFile(join(source, "binary.bin"), Buffer.from([0, 1, 2, 255])));
  await assert.rejects(target.submitPatch(binary), /Binary patches/);

  const symlinkPatch = await candidatePatch(source, async () => symlink("../outside", join(source, "escape")));
  await assert.rejects(target.submitPatch(symlinkPatch), /Symbolic links/);

  await git(source, "mv", "kept.txt", "renamed.txt");
  await git(source, "add", "--all");
  const renamePatch = await git(source, "diff", "--cached", "--find-renames", "HEAD");
  await git(source, "reset", "--hard", "--quiet", "HEAD");
  await assert.rejects(target.submitPatch(renamePatch), /renames/);

  const traversal = "diff --git a/../../outside b/../../outside\n--- a/../../outside\n+++ b/../../outside\n@@ -0,0 +1 @@\n+escape\n";
  await assert.rejects(target.submitPatch(traversal), /invalid path|does not exist|outside/i);
  const absolute = "diff --git a//tmp/outside b//tmp/outside\n--- /dev/null\n+++ b//tmp/outside\n@@ -0,0 +1 @@\n+escape\n";
  await assert.rejects(target.submitPatch(absolute), /invalid path|does not exist|outside|No such file/i);

  const submodule = "diff --git a/vendor b/vendor\nnew file mode 160000\nindex 0000000..1111111\n--- /dev/null\n+++ b/vendor\n@@ -0,0 +1 @@\n+Subproject commit 1111111111111111111111111111111111111111\n";
  await assert.rejects(target.submitPatch(submodule), /submodule|does not exist|patch/i);
  await assert.rejects(target.submitPatch("x".repeat(2_001)), /exceeds 2000 bytes/);
  const tooMany = await candidatePatch(source, async () => {
    await Promise.all(Array.from({ length: 6 }, (_, index) => writeFile(join(source, `many-${index}.txt`), "x\n")));
  });
  await assert.rejects(target.submitPatch(tooMany), /changes 6 files/);

  const valid = await candidatePatch(source, async () => writeFile(join(source, "kept.txt"), "safe change\n"));
  const submitted = await target.submitPatch(valid);
  await rm(join(source, "kept.txt"));
  await symlink(join(root, "outside"), join(source, "kept.txt"));
  await assert.rejects(target.applyPatch(submitted.patchId!), /Symbolic links/);
});

test("cleanup is idempotent and stale workspaces can be reclaimed after restart", async (t) => {
  const { root, source, storage, manager } = await fixture();
  t.after(() => rm(root, { recursive: true, force: true }));

  const disposable = await manager.create("dispose-me", "app");
  const disposableRoot = dirname(disposable.path);
  await disposable.dispose();
  await disposable.dispose();
  await assert.rejects(readdir(disposableRoot), /ENOENT/);

  const stale = await manager.create("stale", "app");
  const metadataPath = join(dirname(stale.path), "workspace.json");
  const metadata = JSON.parse(await readFile(metadataPath, "utf8"));
  metadata.createdAt = "2000-01-01T00:00:00.000Z";
  await writeFile(metadataPath, `${JSON.stringify(metadata)}\n`);

  const restarted = new DisposableWorkspaceManager(storage, { app: source });
  assert.deepEqual(await restarted.reclaimStale(new Date("2020-01-01T00:00:00.000Z")), ["stale"]);
  await assert.rejects(readdir(dirname(stale.path)), /ENOENT/);
  await stale.dispose();
});
