import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, mkdir, readFile, readdir, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { promisify } from "node:util";
import { DisposableWorkspaceManager } from "../src/workspace.js";

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
