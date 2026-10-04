import { z } from "zod";
import { apiUrl } from "./urls";
import { clearToken, getToken, noteChallenge, type Audience } from "./auth";
import {
  acceptedSchema, attachmentDetailSchema, attachmentPageSchema, attachmentsSchema, expandSchema,
  grantResultSchema, grantsSchema, graphSchema, mutationResultSchema, mutationSchema,
  nodeSchema, principalSchema, principalsSchema, relationSchema, reposSchema,
  revokedSchema, searchSchema, sessionSchema, typesSchema, uploadResultSchema,
  vectorStatsSchema, waitlistSchema, webhookSchema, webhookTestSchema, webhooksSchema,
  workspacePageSchema, workspaceResultSchema,
  type Mutation, type RepoInput, type WebhookInput,
} from "./schemas";

const errorSchema = z.object({ code: z.string(), message: z.string() });
type Query = Record<string, string | number | undefined>;

export class ApiError extends Error {
  constructor(public readonly status: number, public readonly code: string, message: string) {
    super(message);
    this.name = "ApiError";
  }
}

interface ApiOptions extends RequestInit {
  audience?: Audience;
  query?: Query;
}

export async function apiFetch<Schema extends z.ZodTypeAny>(
  route: string,
  schema: Schema,
  { audience = "graph", query, ...init }: ApiOptions = {},
): Promise<z.infer<Schema>> {
  const headers = new Headers(init.headers);
  const token = getToken(audience);
  if (token) headers.set("Authorization", `Bearer ${token}`);
  const response = await fetch(apiUrl(route, query), { ...init, headers });
  if (!response.ok) {
    if (response.status === 401 || response.status === 403) {
      noteChallenge(audience, response.headers.get("WWW-Authenticate"));
      if (response.status === 401 && token) clearToken(audience);
    }
    const error = errorSchema.safeParse(await response.json().catch(() => null));
    if (error.success) throw new ApiError(response.status, error.data.code, error.data.message);
    throw new ApiError(response.status, "invalid_response", `The server returned HTTP ${response.status} without a valid error response.`);
  }
  const body: unknown = response.status === 204 ? undefined : await response.json();
  const result = schema.safeParse(body);
  if (!result.success) throw new ApiError(response.status, "invalid_response", `Invalid ${route} response: ${result.error.message}`);
  return result.data;
}

async function jsonRequest<Schema extends z.ZodTypeAny>(
  route: string, schema: Schema, method: "POST" | "PATCH", body: unknown,
  audience: Audience = "graph", query?: Query, signal?: AbortSignal,
): Promise<z.infer<Schema>> {
  return apiFetch(route, schema, {
    method, audience, query, signal,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

async function fetchAttachmentBytes(workspaceId: string, attachmentId: number, signal?: AbortSignal): Promise<Blob> {
  const token = getToken("graph");
  const headers = new Headers();
  if (token) headers.set("Authorization", `Bearer ${token}`);
  const response = await fetch(apiUrl(`attachments/${encodeURIComponent(attachmentId)}/download`, { workspaceId }), { headers, signal });
  if (!response.ok) {
    if (response.status === 401 || response.status === 403) {
      noteChallenge("graph", response.headers.get("WWW-Authenticate"));
      if (response.status === 401 && token) clearToken("graph");
    }
    const error = errorSchema.safeParse(await response.json().catch(() => null));
    if (error.success) throw new ApiError(response.status, error.data.code, error.data.message);
    throw new ApiError(response.status, "invalid_response", `The server returned HTTP ${response.status} without a valid error response.`);
  }
  return response.blob();
}

export type GraphQuery = { workspaceId: string; entityType?: string; offset?: number; limit?: number };
export type SearchQuery = {
  workspaceId: string; q: string; mode: "direct" | "semantic" | "hybrid";
  scope: "nodes" | "relations"; type?: string; from?: string; to?: string;
  relationType?: string; k?: 10 | 20 | 50;
};
export type RelationKey = { workspaceId: string; from: string; to: string; relationType: string };
export type ExpandQuery = { workspaceId: string; name: string; depth: 1 | 2 | 3; direction: "both" | "incoming" | "outgoing" };
export type Upload = { workspaceId: string; entityName: string; filename: string; mime: string; content: Blob };

export const api = {
  session: (workspaceId?: string, audience: Audience = "graph", signal?: AbortSignal) =>
    apiFetch("session", sessionSchema, { audience, query: { workspaceId }, signal }),
  workspaces: (cursor?: string, limit?: number, signal?: AbortSignal) =>
    apiFetch("workspaces", workspacePageSchema, { query: { cursor, limit }, signal }),
  workspace: (id: string, signal?: AbortSignal) =>
    apiFetch(`workspaces/${encodeURIComponent(id)}`, workspaceResultSchema, { signal }),
  createWorkspace: (name: string, signal?: AbortSignal) =>
    jsonRequest("workspaces", workspaceResultSchema, "POST", { name }, "graph", undefined, signal),
  setWorkspaceVisibility: (id: string, visibility: "private" | "public", signal?: AbortSignal) =>
    jsonRequest(`workspaces/${encodeURIComponent(id)}`, workspaceResultSchema, "PATCH", { visibility }, "graph", undefined, signal),
  grants: (id: string, signal?: AbortSignal) =>
    apiFetch(`workspaces/${encodeURIComponent(id)}/grants`, grantsSchema, { signal }),
  grant: (id: string, principalId: string, role: "reader" | "writer", signal?: AbortSignal) =>
    jsonRequest(`workspaces/${encodeURIComponent(id)}/grants`, grantResultSchema, "POST", { principalId, role }, "graph", undefined, signal),
  revokeGrant: (id: string, principalId: string, signal?: AbortSignal) =>
    apiFetch(`workspaces/${encodeURIComponent(id)}/grants/${encodeURIComponent(principalId)}`, revokedSchema, { method: "DELETE", signal }),
  graph: (query: GraphQuery, signal?: AbortSignal) => apiFetch("graph", graphSchema, { query, signal }),
  node: (workspaceId: string, name: string, signal?: AbortSignal) =>
    apiFetch("node", nodeSchema, { query: { workspaceId, name }, signal }),
  relation: (query: RelationKey, signal?: AbortSignal) => apiFetch("relation", relationSchema, { query, signal }),
  expand: (query: ExpandQuery, signal?: AbortSignal) => apiFetch("expand", expandSchema, { query, signal }),
  types: (workspaceId: string, signal?: AbortSignal) => apiFetch("types", typesSchema, { query: { workspaceId }, signal }),
  search: (query: SearchQuery, signal?: AbortSignal) => apiFetch("search", searchSchema, { query, signal }),
  mutate: (workspaceId: string, change: Mutation, signal?: AbortSignal) =>
    jsonRequest("mutations", mutationResultSchema, "POST", { workspaceId, ...mutationSchema.parse(change) }, "graph", undefined, signal),
  attachments: (workspaceId: string, entityName: string, signal?: AbortSignal) =>
    apiFetch("attachments", attachmentsSchema, { query: { workspaceId, entityName }, signal }),
  attachment: (workspaceId: string, attachmentId: number, signal?: AbortSignal) =>
    apiFetch(`attachments/${encodeURIComponent(attachmentId)}`, attachmentDetailSchema, { query: { workspaceId }, signal }),
  attachmentPage: (workspaceId: string, attachmentId: number, page: number, offset = 0, maxChars = 4096, signal?: AbortSignal) =>
    apiFetch(`attachments/${encodeURIComponent(attachmentId)}/pages`, attachmentPageSchema, { query: { workspaceId, page, offset, maxChars }, signal }),
  uploadAttachment: ({ workspaceId, entityName, filename, mime, content }: Upload, signal?: AbortSignal) =>
    apiFetch("attachments", uploadResultSchema, {
      method: "POST", query: { workspaceId, entityName, filename }, signal,
      headers: { "Content-Type": mime }, body: content,
    }),
  attachmentBytes: fetchAttachmentBytes,
  deleteAttachment: (workspaceId: string, attachmentId: number, signal?: AbortSignal) =>
    apiFetch(`attachments/${encodeURIComponent(attachmentId)}`, z.void(), { method: "DELETE", query: { workspaceId }, signal }),
  principals: (signal?: AbortSignal) => apiFetch("principals", principalsSchema, { audience: "admin", signal }),
  createPrincipal: (input: { name: string; iss: string; sub: string; label?: string; scopes: string[] }, signal?: AbortSignal) =>
    jsonRequest("principals", principalSchema, "POST", input, "admin", undefined, signal),
  updatePrincipal: (id: string, patch: { name?: string; label?: string; scopes?: string[] }, signal?: AbortSignal) =>
    jsonRequest(`principals/${encodeURIComponent(id)}`, principalSchema, "PATCH", patch, "admin", undefined, signal),
  deletePrincipal: (id: string, signal?: AbortSignal) =>
    apiFetch(`principals/${encodeURIComponent(id)}`, z.void(), { method: "DELETE", audience: "admin", signal }),
  waitlist: (signal?: AbortSignal) => apiFetch("waitlist", waitlistSchema, { audience: "admin", signal }),
  approveWaitlist: (id: string, scopes?: string[], signal?: AbortSignal) =>
    jsonRequest(`waitlist/${encodeURIComponent(id)}/approve`, principalSchema, "POST", { scopes }, "admin", undefined, signal),
  dismissWaitlist: (id: string, signal?: AbortSignal) =>
    apiFetch(`waitlist/${encodeURIComponent(id)}`, z.void(), { method: "DELETE", audience: "admin", signal }),
  webhooks: (workspaceId: string, signal?: AbortSignal) =>
    apiFetch("webhooks", webhooksSchema, { audience: "admin", query: { workspaceId }, signal }),
  createWebhook: (workspaceId: string, input: WebhookInput, signal?: AbortSignal) =>
    jsonRequest("webhooks", webhookSchema, "POST", input, "admin", { workspaceId }, signal),
  updateWebhook: (workspaceId: string, id: string, patch: Partial<WebhookInput>, signal?: AbortSignal) =>
    jsonRequest(`webhooks/${encodeURIComponent(id)}`, webhookSchema, "PATCH", patch, "admin", { workspaceId }, signal),
  deleteWebhook: (workspaceId: string, id: string, signal?: AbortSignal) =>
    apiFetch(`webhooks/${encodeURIComponent(id)}`, z.void(), { method: "DELETE", audience: "admin", query: { workspaceId }, signal }),
  testWebhook: (workspaceId: string, id: string, signal?: AbortSignal) =>
    apiFetch(`webhooks/${encodeURIComponent(id)}/test`, webhookTestSchema, { method: "POST", audience: "admin", query: { workspaceId }, signal }),
  repos: (signal?: AbortSignal) => apiFetch("repos", reposSchema, { audience: "admin", signal }),
  createRepo: (input: RepoInput, signal?: AbortSignal) =>
    jsonRequest("repos", acceptedSchema, "POST", input, "admin", undefined, signal),
  reindexRepo: (key: string, signal?: AbortSignal) =>
    apiFetch(`repos/${encodeURIComponent(key)}/reindex`, acceptedSchema, { method: "POST", audience: "admin", signal }),
  removeRepo: (key: string, signal?: AbortSignal) =>
    apiFetch(`repos/${encodeURIComponent(key)}`, acceptedSchema, { method: "DELETE", audience: "admin", signal }),
  vectorStats: (workspaceId: string, signal?: AbortSignal) =>
    apiFetch("vectors/stats", vectorStatsSchema, { query: { workspaceId }, signal }),
};

export type ApiClient = typeof api;
