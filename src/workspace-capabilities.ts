import type { CapabilityContext, CapabilityTool, JsonValue } from "./capability-broker.js";
import type { WorkspaceCapabilityRegistry, WorkspaceCapabilityTarget, WorkspacePatchStatus } from "./workspace.js";

const EMPTY_OBJECT_SCHEMA = Object.freeze({
  type: "object",
  additionalProperties: false,
}) as unknown as { readonly [key: string]: JsonValue };

/** Curated host capabilities. None accepts a repository path or command. */
export function createWorkspaceCapabilityTools(registry: WorkspaceCapabilityRegistry): readonly CapabilityTool[] {
  return [
    {
      name: "workspace.read_metadata",
      description: "Read bounded metadata for the authenticated job workspace.",
      inputSchema: EMPTY_OBJECT_SCHEMA,
      policy: "allow",
      normalize(input) { assertExactObject(input, []); return {}; },
      async invoke(_input, context) {
        return await target(registry, context).readMetadata() as unknown as JsonValue;
      },
    },
    {
      name: "workspace.submit_patch",
      description: "Validate and submit a candidate unified Git patch for the authenticated job.",
      inputSchema: {
        type: "object",
        properties: { patch: { type: "string" } },
        required: ["patch"],
        additionalProperties: false,
      },
      policy: "allow",
      normalize(input) {
        const value = assertExactObject(input, ["patch"]);
        if (typeof value.patch !== "string") throw new Error("patch must be a string");
        return { patch: value.patch };
      },
      async invoke(input, context) {
        return statusResult(await target(registry, context).submitPatch((input as { patch: string }).patch));
      },
    },
    {
      name: "workspace.get_patch_status",
      description: "Read the current candidate patch status for the authenticated job.",
      inputSchema: EMPTY_OBJECT_SCHEMA,
      policy: "allow",
      normalize(input) { assertExactObject(input, []); return {}; },
      async invoke(_input, context) {
        return statusResult(await target(registry, context).getPatchStatus());
      },
    },
    {
      name: "workspace.apply_patch",
      description: "Apply one previously submitted candidate patch to its configured repository.",
      inputSchema: {
        type: "object",
        properties: { patchId: { type: "string", pattern: "^[a-f0-9]{64}$" } },
        required: ["patchId"],
        additionalProperties: false,
      },
      policy: "ask",
      normalize(input) {
        const value = assertExactObject(input, ["patchId"]);
        if (typeof value.patchId !== "string" || !/^[a-f0-9]{64}$/.test(value.patchId)) {
          throw new Error("patchId must be a SHA-256 identifier");
        }
        return { patchId: value.patchId };
      },
      async invoke(input, context) {
        return statusResult(await target(registry, context).applyPatch((input as { patchId: string }).patchId));
      },
    },
  ];
}

export const WORKSPACE_CAPABILITY_NAMES = Object.freeze([
  "workspace.read_metadata",
  "workspace.submit_patch",
  "workspace.get_patch_status",
  "workspace.apply_patch",
]);

function target(registry: WorkspaceCapabilityRegistry, context: CapabilityContext): WorkspaceCapabilityTarget {
  const workspace = registry.resolve(context.jobId, context.repositoryScope);
  if (!workspace) throw new Error("No active workspace matches this capability lease");
  return workspace;
}

function assertExactObject(input: unknown, keys: readonly string[]): Record<string, unknown> {
  if (typeof input !== "object" || input === null || Array.isArray(input)) throw new Error("arguments must be an object");
  const value = input as Record<string, unknown>;
  const actual = Object.keys(value).sort();
  const expected = [...keys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new Error("arguments contain missing or unknown fields");
  }
  return value;
}

function statusResult(status: WorkspacePatchStatus): JsonValue {
  return {
    state: status.state,
    ...(status.patchId ? { patchId: status.patchId } : {}),
    files: status.files.map((file) => ({ path: file.path, status: file.status })),
    bytes: status.bytes,
    ...(status.patch ? { patch: status.patch } : {}),
    ...(status.appliedAt ? { appliedAt: status.appliedAt } : {}),
  };
}
