import { constants } from "node:fs";
import { createHash } from "node:crypto";
import {
  chmod,
  lstat,
  mkdir,
  open,
  readdir,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { dirname, isAbsolute, join, normalize, resolve, sep } from "node:path";
import { promisify } from "node:util";
import { execFile, spawn } from "node:child_process";

const execFileAsync = promisify(execFile);
const METADATA_FILE = "workspace.json";
const METADATA_VERSION = 1;

export interface WorkspacePatchFile {
  path: string;
  status: "added" | "modified" | "deleted";
}

export interface WorkspacePatch {
  patch: string;
  files: readonly WorkspacePatchFile[];
}

export interface PatchLimits {
  maxBytes: number;
  maxFiles: number;
}

export interface JobWorkspace {
  readonly jobId: string;
  /** Disposable repository content. This is never the configured checkout. */
  readonly path: string;
  exportPatch(limits?: Partial<PatchLimits>): Promise<WorkspacePatch>;
  dispose(): Promise<void>;
}

export interface WorkspaceProvider {
  readonly aliases: readonly string[];
  create(jobId: string, repositoryAlias: string): Promise<JobWorkspace>;
}

export interface WorkspacePatchStatus {
  state: "none" | "submitted" | "applied";
  patchId?: string;
  files: readonly WorkspacePatchFile[];
  bytes: number;
  patch?: string;
  appliedAt?: string;
}

export interface WorkspaceCapabilityTarget {
  readonly jobId: string;
  readonly repositoryAlias: string;
  readMetadata(): Promise<{ jobId: string; repositoryAlias: string; patchLimits: PatchLimits }>;
  submitPatch(patch: string): Promise<WorkspacePatchStatus>;
  getPatchStatus(): Promise<WorkspacePatchStatus>;
  applyPatch(patchId: string): Promise<WorkspacePatchStatus>;
}

export interface WorkspaceCapabilityRegistry {
  resolve(jobId: string, repositoryAlias: string): WorkspaceCapabilityTarget | undefined;
}

export interface WorkspaceManagerOptions {
  maxSnapshotFiles?: number;
  maxSnapshotBytes?: number;
  maxFileBytes?: number;
  patchLimits?: Partial<PatchLimits>;
  allowBinaryPatches?: boolean;
}

interface WorkspaceMetadata {
  version: number;
  jobId: string;
  repositoryAlias: string;
  createdAt: string;
}

interface ScannedFile {
  path: string;
  mode: number;
  size: number;
}

interface ScanLimits {
  maxFiles: number;
  maxBytes: number;
  maxFileBytes: number;
}

const DEFAULT_SCAN_LIMITS: ScanLimits = {
  maxFiles: 20_000,
  maxBytes: 500 * 1024 * 1024,
  maxFileBytes: 100 * 1024 * 1024,
};

const DEFAULT_PATCH_LIMITS: PatchLimits = {
  maxBytes: 2 * 1024 * 1024,
  maxFiles: 200,
};

export class DisposableWorkspaceManager implements WorkspaceProvider, WorkspaceCapabilityRegistry {
  readonly aliases: readonly string[];
  private readonly root: string;
  private readonly scanLimits: ScanLimits;
  private readonly patchLimits: PatchLimits;
  private readonly allowBinaryPatches: boolean;
  private readonly active = new Map<string, DisposableJobWorkspace>();

  constructor(
    root: string,
    private readonly repositories: Readonly<Record<string, string>>,
    options: WorkspaceManagerOptions = {},
  ) {
    this.root = resolve(root);
    this.aliases = Object.freeze(Object.keys(repositories).sort());
    this.scanLimits = {
      maxFiles: options.maxSnapshotFiles ?? DEFAULT_SCAN_LIMITS.maxFiles,
      maxBytes: options.maxSnapshotBytes ?? DEFAULT_SCAN_LIMITS.maxBytes,
      maxFileBytes: options.maxFileBytes ?? DEFAULT_SCAN_LIMITS.maxFileBytes,
    };
    this.patchLimits = {
      maxBytes: options.patchLimits?.maxBytes ?? DEFAULT_PATCH_LIMITS.maxBytes,
      maxFiles: options.patchLimits?.maxFiles ?? DEFAULT_PATCH_LIMITS.maxFiles,
    };
    this.allowBinaryPatches = options.allowBinaryPatches ?? false;
    assertPositiveLimits(this.scanLimits);
    assertPositiveLimits(this.patchLimits);
  }

  async create(jobId: string, repositoryAlias: string): Promise<JobWorkspace> {
    validateJobId(jobId);
    const configuredPath = this.repositories[repositoryAlias];
    if (!configuredPath) throw new Error(`Unknown repository alias '${repositoryAlias}'`);
    await this.ensureRoot();

    const source = await validateRepository(configuredPath);
    const files = await listRepositoryFiles(source, this.scanLimits);
    const jobRoot = join(this.root, `job-${jobId}`);
    const controlPath = join(jobRoot, "control");
    const workspacePath = join(jobRoot, "workspace");

    await mkdir(jobRoot, { mode: 0o700 });
    try {
      const metadata: WorkspaceMetadata = {
        version: METADATA_VERSION,
        jobId,
        repositoryAlias,
        createdAt: new Date().toISOString(),
      };
      await writeFile(join(jobRoot, METADATA_FILE), `${JSON.stringify(metadata)}\n`, { flag: "wx", mode: 0o600 });
      await Promise.all([
        mkdir(controlPath, { mode: 0o700 }),
        mkdir(workspacePath, { mode: 0o700 }),
      ]);
      await copyFiles(source, controlPath, files, this.scanLimits.maxFileBytes);
      await copyFiles(controlPath, workspacePath, files, this.scanLimits.maxFileBytes);
      await initializeBaseline(controlPath);
      const workspace = new DisposableJobWorkspace(
        jobId,
        repositoryAlias,
        source,
        jobRoot,
        controlPath,
        workspacePath,
        this.scanLimits,
        this.patchLimits,
        this.allowBinaryPatches,
        () => this.active.delete(jobId),
      );
      this.active.set(jobId, workspace);
      return workspace;
    } catch (error) {
      await rm(jobRoot, { recursive: true, force: true });
      throw error;
    }
  }

  resolve(jobId: string, repositoryAlias: string): WorkspaceCapabilityTarget | undefined {
    const workspace = this.active.get(jobId);
    return workspace?.repositoryAlias === repositoryAlias ? workspace : undefined;
  }

  /** Removes recognized job directories whose trusted metadata is old enough. */
  async reclaimStale(olderThan: Date): Promise<string[]> {
    await this.ensureRoot();
    const reclaimed: string[] = [];
    for (const entry of await readdir(this.root, { withFileTypes: true })) {
      if (!entry.isDirectory() || !entry.name.startsWith("job-")) continue;
      const jobRoot = join(this.root, entry.name);
      const metadataPath = join(jobRoot, METADATA_FILE);
      try {
        const metadataInfo = await lstat(metadataPath);
        if (!metadataInfo.isFile() || metadataInfo.isSymbolicLink()) continue;
        let metadata: Partial<WorkspaceMetadata>;
        try {
          metadata = JSON.parse(await readFile(metadataPath, "utf8")) as Partial<WorkspaceMetadata>;
        } catch {
          continue;
        }
        if (metadata.version !== METADATA_VERSION || typeof metadata.jobId !== "string" || !isValidJobId(metadata.jobId)) continue;
        if (`job-${metadata.jobId}` !== entry.name || typeof metadata.createdAt !== "string") continue;
        const createdAt = new Date(metadata.createdAt);
        if (Number.isNaN(createdAt.getTime()) || createdAt >= olderThan) continue;
        await rm(jobRoot, { recursive: true, force: true });
        reclaimed.push(metadata.jobId);
      } catch (error) {
        if (!isMissing(error)) throw error;
      }
    }
    return reclaimed.sort();
  }

  private async ensureRoot(): Promise<void> {
    await mkdir(this.root, { recursive: true, mode: 0o700 });
    const info = await lstat(this.root);
    if (!info.isDirectory() || info.isSymbolicLink()) throw new Error("Workspace storage root must be a real directory");
    await chmod(this.root, 0o700);
  }
}

class DisposableJobWorkspace implements JobWorkspace, WorkspaceCapabilityTarget {
  private disposed = false;
  private operation = Promise.resolve();
  private candidate?: { patchId: string; patch: string; files: readonly WorkspacePatchFile[]; bytes: number; appliedAt?: string };

  constructor(
    readonly jobId: string,
    readonly repositoryAlias: string,
    private readonly sourcePath: string,
    private readonly jobRoot: string,
    private readonly controlPath: string,
    readonly path: string,
    private readonly scanLimits: ScanLimits,
    private readonly defaultPatchLimits: PatchLimits,
    private readonly allowBinaryPatches: boolean,
    private readonly onDispose: () => void,
  ) {}

  exportPatch(overrides: Partial<PatchLimits> = {}): Promise<WorkspacePatch> {
    const limits = {
      maxBytes: overrides.maxBytes ?? this.defaultPatchLimits.maxBytes,
      maxFiles: overrides.maxFiles ?? this.defaultPatchLimits.maxFiles,
    };
    assertPositiveLimits(limits);
    return this.exclusive(async () => {
      if (this.disposed) throw new Error("Workspace is disposed");
      const files = await scanTree(this.path, this.scanLimits);
      await resetControlTree(this.controlPath);
      await copyFiles(this.path, this.controlPath, files, this.scanLimits.maxFileBytes);
      await git(this.controlPath, ["add", "--all", "--", "."]);

      const manifestOutput = await git(this.controlPath, [
        "diff", "--cached", "--name-status", "-z", "--no-renames", "HEAD", "--", ".",
      ]);
      const manifest = parseManifest(manifestOutput);
      if (manifest.length > limits.maxFiles) {
        throw new Error(`Patch changes ${manifest.length} files; limit is ${limits.maxFiles}`);
      }

      let patch: string;
      try {
        patch = await git(this.controlPath, [
          "diff", "--cached", "--binary", "--full-index", "--no-ext-diff", "--no-textconv", "--no-renames", "HEAD", "--", ".",
        ], limits.maxBytes + 1);
      } catch (error) {
        if (isMaxBuffer(error)) throw new Error(`Patch exceeds ${limits.maxBytes} bytes`);
        throw error;
      }
      if (Buffer.byteLength(patch, "utf8") > limits.maxBytes) {
        throw new Error(`Patch exceeds ${limits.maxBytes} bytes`);
      }
      return { patch, files: manifest };
    });
  }

  readMetadata(): Promise<{ jobId: string; repositoryAlias: string; patchLimits: PatchLimits }> {
    return this.exclusive(async () => {
      this.assertActive();
      return { jobId: this.jobId, repositoryAlias: this.repositoryAlias, patchLimits: { ...this.defaultPatchLimits } };
    });
  }

  submitPatch(patch: string): Promise<WorkspacePatchStatus> {
    return this.exclusive(async () => {
      this.assertActive();
      if (!patch || patch.includes("\0")) throw new Error("Patch must be non-empty UTF-8 text");
      if (Buffer.byteLength(patch, "utf8") > this.defaultPatchLimits.maxBytes) {
        throw new Error(`Patch exceeds ${this.defaultPatchLimits.maxBytes} bytes`);
      }
      rejectUnsafePatchDirectives(patch);
      await resetControlToBaseline(this.controlPath);
      await gitWithInput(this.controlPath, ["apply", "--check", "--binary", "--whitespace=nowarn", "-"], patch);
      await gitWithInput(this.controlPath, ["apply", "--binary", "--whitespace=nowarn", "-"], patch);
      const files = await scanTree(this.controlPath, this.scanLimits, new Set([".git"]));
      await git(this.controlPath, ["add", "--all", "--", "."]);
      const manifest = parseManifest(await git(this.controlPath, [
        "diff", "--cached", "--name-status", "-z", "--no-renames", "HEAD", "--", ".",
      ]));
      if (manifest.length > this.defaultPatchLimits.maxFiles) {
        throw new Error(`Patch changes ${manifest.length} files; limit is ${this.defaultPatchLimits.maxFiles}`);
      }
      const canonical = await boundedPatch(this.controlPath, this.defaultPatchLimits.maxBytes);
      if (!canonical || manifest.length === 0) throw new Error("Patch makes no changes");
      if (!this.allowBinaryPatches && canonical.includes("GIT binary patch")) throw new Error("Binary patches are not allowed");
      const patchId = createHash("sha256").update(canonical).digest("hex");
      this.candidate = { patchId, patch: canonical, files: manifest, bytes: Buffer.byteLength(canonical, "utf8") };
      return this.status();
    });
  }

  getPatchStatus(): Promise<WorkspacePatchStatus> {
    return this.exclusive(async () => {
      this.assertActive();
      return this.status(true);
    });
  }

  applyPatch(patchId: string): Promise<WorkspacePatchStatus> {
    return this.exclusive(async () => {
      this.assertActive();
      const candidate = this.candidate;
      if (!candidate || candidate.patchId !== patchId) throw new Error("Unknown or stale patch ID");
      if (candidate.appliedAt) return this.status();
      const source = await validateRepository(this.sourcePath);
      if (source !== this.sourcePath) throw new Error("Configured repository identity changed");
      await listRepositoryFiles(source, this.scanLimits);
      await assertSafeTargetPaths(source, candidate.files);
      await gitWithInput(source, ["apply", "--check", "--binary", "--whitespace=nowarn", "-"], candidate.patch);
      await gitWithInput(source, ["apply", "--binary", "--whitespace=nowarn", "-"], candidate.patch);
      candidate.appliedAt = new Date().toISOString();
      return this.status();
    });
  }

  dispose(): Promise<void> {
    return this.exclusive(async () => {
      if (this.disposed) return;
      this.disposed = true;
      this.onDispose();
      await rm(this.jobRoot, { recursive: true, force: true });
    });
  }

  private status(includePatch = false): WorkspacePatchStatus {
    if (!this.candidate) return { state: "none", files: [], bytes: 0 };
    return {
      state: this.candidate.appliedAt ? "applied" : "submitted",
      patchId: this.candidate.patchId,
      files: this.candidate.files,
      bytes: this.candidate.bytes,
      ...(includePatch ? { patch: this.candidate.patch } : {}),
      ...(this.candidate.appliedAt ? { appliedAt: this.candidate.appliedAt } : {}),
    };
  }

  private assertActive(): void {
    if (this.disposed) throw new Error("Workspace is disposed");
  }

  private async exclusive<T>(operation: () => Promise<T>): Promise<T> {
    const previous = this.operation;
    let release!: () => void;
    this.operation = new Promise<void>((resolveOperation) => { release = resolveOperation; });
    await previous;
    try {
      return await operation();
    } finally {
      release();
    }
  }
}

async function validateRepository(configuredPath: string): Promise<string> {
  if (!isAbsolute(configuredPath)) throw new Error("Configured repository paths must be absolute");
  const configuredInfo = await lstat(configuredPath);
  if (!configuredInfo.isDirectory() || configuredInfo.isSymbolicLink()) {
    throw new Error("Configured repository must be a real directory");
  }
  const source = await realpath(configuredPath);
  const topLevel = (await git(source, ["rev-parse", "--show-toplevel"])).trim();
  if (await realpath(topLevel) !== source) throw new Error("Repository alias must identify the Git top-level directory");
  return source;
}

async function listRepositoryFiles(source: string, limits: ScanLimits): Promise<ScannedFile[]> {
  const staged = await git(source, ["ls-files", "--stage", "-z"]);
  for (const record of splitNull(staged)) {
    const tab = record.indexOf("\t");
    if (tab < 0) throw new Error("Git returned malformed index metadata");
    const mode = record.slice(0, record.indexOf(" "));
    if (mode === "160000") throw new Error("Repositories with submodules are not supported");
  }

  const output = await git(source, ["ls-files", "--cached", "--others", "--exclude-standard", "-z"]);
  const deleted = new Set(splitNull(await git(source, ["ls-files", "--deleted", "-z"])));
  const paths = splitNull(output).filter((path) => !deleted.has(path)).sort();
  const files: ScannedFile[] = [];
  let bytes = 0;
  for (const path of paths) {
    validateRelativePath(path);
    if (path.split("/").includes(".git") || path.endsWith(".gitmodules")) {
      throw new Error(`Repository metadata path is not allowed: ${path}`);
    }
    const absolute = containedPath(source, path);
    const info = await lstat(absolute);
    if (info.isSymbolicLink()) throw new Error(`Symbolic links are not allowed in workspaces: ${path}`);
    if (!info.isFile() || info.nlink !== 1) throw new Error(`Only regular, non-linked files are allowed: ${path}`);
    if (info.size > limits.maxFileBytes) throw new Error(`Repository file exceeds size limit: ${path}`);
    bytes += info.size;
    if (files.length + 1 > limits.maxFiles) throw new Error(`Repository exceeds ${limits.maxFiles} files`);
    if (bytes > limits.maxBytes) throw new Error(`Repository exceeds ${limits.maxBytes} bytes`);
    files.push({ path, mode: info.mode, size: info.size });
  }
  return files;
}

async function scanTree(root: string, limits: ScanLimits, excludedTopLevel = new Set<string>()): Promise<ScannedFile[]> {
  const files: ScannedFile[] = [];
  let bytes = 0;
  const visit = async (directory: string, prefix: string): Promise<void> => {
    const entries = (await readdir(directory, { withFileTypes: true })).sort((a, b) => a.name.localeCompare(b.name));
    for (const entry of entries) {
      if (!prefix && excludedTopLevel.has(entry.name)) continue;
      const path = prefix ? `${prefix}/${entry.name}` : entry.name;
      validateRelativePath(path);
      if (entry.name === ".git" || entry.name === ".gitmodules") throw new Error(`Workspace metadata path is not allowed: ${path}`);
      const absolute = containedPath(root, path);
      const info = await lstat(absolute);
      if (info.isSymbolicLink()) throw new Error(`Symbolic links are not allowed in workspaces: ${path}`);
      if (info.isDirectory()) {
        await visit(absolute, path);
        continue;
      }
      if (!info.isFile() || info.nlink !== 1) throw new Error(`Only regular, non-linked files are allowed: ${path}`);
      if (info.size > limits.maxFileBytes) throw new Error(`Workspace file exceeds size limit: ${path}`);
      bytes += info.size;
      if (files.length + 1 > limits.maxFiles) throw new Error(`Workspace exceeds ${limits.maxFiles} files`);
      if (bytes > limits.maxBytes) throw new Error(`Workspace exceeds ${limits.maxBytes} bytes`);
      files.push({ path, mode: info.mode, size: info.size });
    }
  };
  await visit(root, "");
  return files;
}

async function assertSafeTargetPaths(root: string, files: readonly WorkspacePatchFile[]): Promise<void> {
  for (const file of files) {
    const parts = file.path.split("/");
    for (let index = 1; index <= parts.length; index++) {
      const path = containedPath(root, parts.slice(0, index).join("/"));
      let info;
      try { info = await lstat(path); } catch (error) {
        if (isMissing(error)) break;
        throw error;
      }
      if (info.isSymbolicLink()) throw new Error(`Symbolic links are not allowed in patch targets: ${file.path}`);
      if (index < parts.length && !info.isDirectory()) throw new Error(`Patch target parent is not a directory: ${file.path}`);
      if (index === parts.length && !info.isDirectory() && (!info.isFile() || info.nlink !== 1)) {
        throw new Error(`Patch target is not a regular file: ${file.path}`);
      }
    }
  }
}

async function copyFiles(source: string, destination: string, files: readonly ScannedFile[], maxFileBytes: number): Promise<void> {
  for (const file of files) {
    const sourcePath = containedPath(source, file.path);
    const destinationPath = containedPath(destination, file.path);
    await mkdir(dirname(destinationPath), { recursive: true, mode: 0o700 });
    const handle = await open(sourcePath, constants.O_RDONLY | constants.O_NOFOLLOW);
    try {
      const current = await handle.stat();
      if (!current.isFile() || current.nlink !== 1 || current.size > maxFileBytes || current.size !== file.size) {
        throw new Error(`File changed or became unsafe while copying: ${file.path}`);
      }
      await writeFile(destinationPath, await handle.readFile(), { mode: file.mode & 0o111 ? 0o700 : 0o600 });
    } finally {
      await handle.close();
    }
  }
}

async function initializeBaseline(controlPath: string): Promise<void> {
  await git(controlPath, ["init", "--quiet"]);
  await git(controlPath, ["add", "--all", "--", "."]);
  await git(controlPath, [
    "-c", "user.name=Pocket Agent", "-c", "user.email=worker@invalid", "commit", "--quiet", "--allow-empty", "-m", "workspace baseline",
  ]);
}

async function resetControlToBaseline(controlPath: string): Promise<void> {
  await git(controlPath, ["reset", "--hard", "--quiet", "HEAD"]);
  await git(controlPath, ["clean", "-ffdqx"]);
}

async function resetControlTree(controlPath: string): Promise<void> {
  await resetControlToBaseline(controlPath);
  for (const entry of await readdir(controlPath, { withFileTypes: true })) {
    if (entry.name === ".git") continue;
    await rm(join(controlPath, entry.name), { recursive: true, force: true });
  }
}

function parseManifest(output: string): WorkspacePatchFile[] {
  const tokens = splitNull(output);
  if (tokens.length % 2 !== 0) throw new Error("Git returned a malformed changed-file manifest");
  const files: WorkspacePatchFile[] = [];
  for (let index = 0; index < tokens.length; index += 2) {
    const code = tokens[index]!;
    const path = tokens[index + 1]!;
    validateRelativePath(path);
    if (!["A", "D", "M", "T"].includes(code)) throw new Error(`Unsupported Git change status: ${code}`);
    const status = code === "A" ? "added" : code === "D" ? "deleted" : "modified";
    files.push({ path, status });
  }
  return files;
}

async function boundedPatch(cwd: string, maxBytes: number): Promise<string> {
  let patch: string;
  try {
    patch = await git(cwd, [
      "diff", "--cached", "--binary", "--full-index", "--no-ext-diff", "--no-textconv", "--no-renames", "HEAD", "--", ".",
    ], maxBytes + 1);
  } catch (error) {
    if (isMaxBuffer(error)) throw new Error(`Patch exceeds ${maxBytes} bytes`);
    throw error;
  }
  if (Buffer.byteLength(patch, "utf8") > maxBytes) throw new Error(`Patch exceeds ${maxBytes} bytes`);
  return patch;
}

function rejectUnsafePatchDirectives(patch: string): void {
  if (/^(rename|copy) (from|to) /m.test(patch) || /^similarity index /m.test(patch)) {
    throw new Error("Patch renames and copies are not supported");
  }
  if (/^(new file mode|old mode|new mode) 160000$/m.test(patch)) {
    throw new Error("Patch submodules are not supported");
  }
}

async function gitWithInput(cwd: string, args: readonly string[], input: string): Promise<string> {
  const hardenedArgs = [
    "-c", "core.fsmonitor=false",
    "-c", "core.hooksPath=/dev/null",
    "-c", "credential.helper=",
    ...args,
  ];
  return new Promise<string>((resolveGit, rejectGit) => {
    const child = spawn("git", hardenedArgs, {
      cwd,
      env: gitEnvironment(),
      stdio: ["pipe", "pipe", "pipe"],
    });
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let bytes = 0;
    const collect = (destination: Buffer[]) => (chunk: Buffer) => {
      bytes += chunk.length;
      if (bytes > 16 * 1024 * 1024) {
        child.kill("SIGKILL");
        return;
      }
      destination.push(chunk);
    };
    child.stdout.on("data", collect(stdout));
    child.stderr.on("data", collect(stderr));
    child.stdin.on("error", () => { /* process exit reports the failure */ });
    child.once("error", rejectGit);
    child.once("close", (code) => {
      if (bytes > 16 * 1024 * 1024) return rejectGit(new Error("Git output exceeded its limit"));
      if (code !== 0) return rejectGit(new Error(Buffer.concat(stderr).toString("utf8").trim() || `Git exited ${code}`));
      resolveGit(Buffer.concat(stdout).toString("utf8"));
    });
    child.stdin.end(input, "utf8");
  });
}

async function git(cwd: string, args: readonly string[], maxBuffer = 16 * 1024 * 1024): Promise<string> {
  const hardenedArgs = [
    "-c", "core.fsmonitor=false",
    "-c", "core.hooksPath=/dev/null",
    "-c", "credential.helper=",
    ...args,
  ];
  const { stdout } = await execFileAsync("git", hardenedArgs, {
    cwd,
    encoding: "utf8",
    maxBuffer,
    env: gitEnvironment(),
  });
  return stdout;
}

function gitEnvironment(): NodeJS.ProcessEnv {
  return {
    PATH: process.env.PATH,
    HOME: "/nonexistent",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_ATTR_NOSYSTEM: "1",
    GIT_TERMINAL_PROMPT: "0",
    GIT_OPTIONAL_LOCKS: "0",
  };
}

function validateJobId(jobId: string): void {
  if (!isValidJobId(jobId)) throw new Error("Invalid workspace job identity");
}

function isValidJobId(jobId: string): boolean {
  return /^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$/.test(jobId);
}

function validateRelativePath(path: string): void {
  if (!path || path.includes("\0") || isAbsolute(path) || normalize(path) !== path || path.split(/[\\/]/).some((part) => part === ".." || part === "." || part === "")) {
    throw new Error(`Unsafe workspace path: ${JSON.stringify(path)}`);
  }
}

function containedPath(root: string, path: string): string {
  const candidate = resolve(root, path);
  if (candidate !== root && !candidate.startsWith(`${root}${sep}`)) throw new Error(`Path escapes workspace: ${path}`);
  return candidate;
}

function splitNull(value: string): string[] {
  if (!value) return [];
  const parts = value.split("\0");
  if (parts.at(-1) === "") parts.pop();
  return parts;
}

function assertPositiveLimits(limits: object): void {
  for (const [name, value] of Object.entries(limits)) {
    if (!Number.isSafeInteger(value) || value <= 0) throw new Error(`${name} must be a positive integer`);
  }
}

function isMissing(error: unknown): boolean {
  return error instanceof Error && "code" in error && error.code === "ENOENT";
}

function isMaxBuffer(error: unknown): boolean {
  return error instanceof Error && (/maxBuffer/.test(error.message) || "code" in error && error.code === "ERR_CHILD_PROCESS_STDIO_MAXBUFFER");
}
