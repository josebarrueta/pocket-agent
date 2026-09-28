# Disposable workspaces

`DisposableWorkspaceManager` is the trusted host module that resolves a configured repository alias into per-job content. Chat and worker inputs never select a host path.

## Snapshot flow

1. Resolve the exact alias from startup configuration.
2. Require its path to be an absolute, real directory and the Git top level.
3. Enumerate tracked and non-ignored untracked files with Git. Deleted tracked files are omitted.
4. Reject submodules, `.git`/`.gitmodules`, symbolic links, hard links, special files, path traversal, excessive files, and excessive bytes.
5. Copy regular files through `O_NOFOLLOW` into two broker-owned directories: an inaccessible baseline repository and a disposable workspace.
6. Pass only the disposable path to the sandbox adapter. The Docker adapter imports that content into its own volume; it must never mount the configured checkout.

Ignored files are not copied. This keeps common local credentials and build artifacts out of snapshots, but repository owners must still avoid committing secrets.

## Patch export

The baseline is initialized as a fresh Git repository with global and system configuration disabled. On export, the host validates the complete disposable tree again, copies it into the private baseline worktree, and asks Git for a full-index binary patch and a no-renames changed-file manifest.

Default limits are:

- 20,000 workspace files;
- 500 MiB total workspace content;
- 100 MiB per file;
- 200 changed files per patch;
- 2 MiB patch output.

Callers may lower patch limits per export. Exceeding any limit fails closed. Patch paths are relative and validated before being returned. Applying a candidate patch to the configured checkout is deliberately not part of this module; that requires a separate capability and approval.

## Lifecycle and recovery

Each job directory has trusted metadata outside the worker-visible workspace. `dispose()` recursively removes the job directory and is idempotent. The controller disposes workspaces on cancellation, worker failure, sandbox creation failure, and daemon shutdown.

At startup, the daemon calls `reclaimStale()` before accepting jobs. Only directories with recognized metadata, matching validated job identity, and a creation time older than the supplied threshold are removed. Unknown directories and symbolic links are left untouched for manual inspection.

Patch export and disposal are serialized. A sandbox adapter must stop the worker before final export/disposal so a malicious process cannot race filesystem validation.
