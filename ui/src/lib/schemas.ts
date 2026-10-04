import { z } from "zod";

const role = z.enum(["owner", "writer", "reader", "public"]);
const visibility = z.enum(["private", "public"]);
const attributes = z.record(z.string());
const triple = z.object({ from: z.string(), to: z.string(), relationType: z.string() });
const observation = z.object({
  observationId: z.number().int(),
  body: z.string(),
  createdAtUs: z.number().nullable(),
  occurredAtUs: z.number().nullable(),
  originEntityName: z.string().nullable(),
});
const observationInput = z.object({ body: z.string(), occurredAtUs: z.number().int().nonnegative().optional() });

export const sessionSchema = z.object({
  scopes: z.array(z.string()),
  principalName: z.string().nullable(),
  workspaceRole: role.nullable(),
  features: z.object({
    vectors: z.boolean(), attachments: z.boolean(), code: z.boolean(), webhooks: z.boolean(),
  }),
});
export type Session = z.infer<typeof sessionSchema>;

export const workspaceSchema = z.object({
  workspaceId: z.string(), name: z.string(), visibility, role, isDefault: z.boolean(),
});
export type Workspace = z.infer<typeof workspaceSchema>;
export const workspacePageSchema = z.object({ workspaces: z.array(workspaceSchema), nextCursor: z.string().nullable() });
export const workspaceResultSchema = z.object({ workspace: workspaceSchema });
export const grantSchema = z.object({ principalId: z.string(), role: z.enum(["reader", "writer"]) });
export const grantsSchema = z.object({ grants: z.array(grantSchema) });
export const grantResultSchema = z.object({ grant: grantSchema });
export const revokedSchema = z.object({ revoked: z.boolean() });

export const graphSchema = z.object({
  entities: z.array(z.object({ name: z.string(), entityType: z.string(), obsCount: z.number().int() })),
  relations: z.array(triple),
  entityTypes: z.array(z.object({ type: z.string(), count: z.number().int() })),
  stats: z.object({ entities: z.number().int(), relations: z.number().int() }),
  page: z.object({ offset: z.number().int(), limit: z.number().int(), returned: z.number().int(), hasMore: z.boolean() }),
});
export type GraphPage = z.infer<typeof graphSchema>;

// The approved detail contract includes the full incident graph. The server
// must supply these members before the Graph page calls this method.
export const nodeSchema = z.object({
  name: z.string(), entityType: z.string(), observations: z.array(observation),
  attributes: attributes.optional(), relations: z.array(triple), neighbors: z.array(z.string()),
  degree: z.object({ in: z.number().int(), out: z.number().int() }),
});
export type NodeDetail = z.infer<typeof nodeSchema>;
export const relationSchema = triple.extend({ observations: z.array(observation), attributes });
export type RelationDetail = z.infer<typeof relationSchema>;
export const expandSchema = z.object({
  entities: z.array(z.object({ name: z.string(), entityType: z.string(), observations: z.array(observation) })),
  relations: z.array(triple),
});
export const typesSchema = z.object({
  entities: z.array(z.object({ type: z.string(), count: z.number().int(), desc: z.string().optional() })),
  relations: z.array(z.object({ type: z.string(), count: z.number().int(), desc: z.string().optional() })),
});
export type TypeList = z.infer<typeof typesSchema>;

const score = z.object({ score: z.number().optional(), textScore: z.number().optional(), vecScore: z.number().optional() });
export const searchHitSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("entity"), name: z.string(), entityType: z.string(), snippet: z.string().optional() }).merge(score),
  triple.extend({ kind: z.literal("relation"), snippet: z.string().optional() }).merge(score),
  z.object({
    kind: z.literal("attachment"), attachmentId: z.number().int(), entityName: z.string(),
    filename: z.string(), page: z.number().int(), excerpt: z.string(), score: z.number(),
    textScore: z.number().optional(), vecScore: z.number().optional(),
  }),
]);
export type SearchHit = z.infer<typeof searchHitSchema>;
export const searchSchema = z.object({
  results: z.array(searchHitSchema), count: z.number().int(), elapsedMs: z.number().nonnegative(),
});
export type SearchResults = z.infer<typeof searchSchema>;

const observationTarget = z.object({ entityName: z.string() });
const relationTarget = triple;
const observedRelation = triple.extend({ body: z.string(), occurredAtUs: z.number().int().nonnegative().optional() });
export const mutationSchema = z.discriminatedUnion("operation", [
  z.object({ operation: z.literal("createEntity"), payload: z.object({ name: z.string(), entityType: z.string(), observations: z.array(observationInput), attributes: attributes.optional() }) }),
  z.object({ operation: z.literal("renameEntity"), payload: z.object({ oldName: z.string(), newName: z.string() }) }),
  z.object({ operation: z.literal("mergeEntities"), payload: z.object({ source: z.string(), target: z.string() }) }),
  z.object({ operation: z.literal("deleteEntity"), payload: z.object({ name: z.string() }) }),
  z.object({ operation: z.literal("createRelation"), payload: triple.extend({ observations: z.array(observationInput).optional(), attributes: attributes.optional() }) }),
  z.object({ operation: z.literal("deleteRelation"), payload: relationTarget }),
  z.object({ operation: z.literal("reverseRelation"), payload: relationTarget }),
  z.object({ operation: z.literal("changeRelationType"), payload: triple.extend({ newRelationType: z.string() }) }),
  z.object({ operation: z.literal("setEntityAttributes"), payload: observationTarget.extend({ attributes }) }),
  z.object({ operation: z.literal("deleteEntityAttributes"), payload: observationTarget.extend({ keys: z.array(z.string()) }) }),
  z.object({ operation: z.literal("setRelationAttributes"), payload: triple.extend({ attributes }) }),
  z.object({ operation: z.literal("deleteRelationAttributes"), payload: triple.extend({ keys: z.array(z.string()) }) }),
  z.object({ operation: z.literal("addObservation"), payload: observationTarget.merge(observationInput) }),
  z.object({ operation: z.literal("deleteObservation"), payload: observationTarget.extend({ observationId: z.number().int() }) }),
  z.object({ operation: z.literal("editObservation"), payload: observationTarget.merge(observationInput).extend({ observationId: z.number().int() }) }),
  z.object({ operation: z.literal("addRelationObservation"), payload: observedRelation }),
  z.object({ operation: z.literal("deleteRelationObservation"), payload: triple.extend({ observationId: z.number().int() }) }),
]);
export type Mutation = z.infer<typeof mutationSchema>;
export const mutationResultSchema = z.object({ ok: z.literal(true) });

export const attachmentSchema = z.object({
  attachmentId: z.number().int(), filename: z.string(), mime: z.string(), sizeBytes: z.number().int(),
  status: z.enum(["uploaded", "extracting", "ready", "error"]), revision: z.number().int(),
  errorStage: z.string().nullable(), lastError: z.string().nullable(), pageCount: z.number().int().nullable(),
});
export type Attachment = z.infer<typeof attachmentSchema>;
export const attachmentsSchema = z.object({ attachments: z.array(attachmentSchema) });
export const attachmentDetailSchema = attachmentSchema.extend({ entityName: z.string() });
export const uploadResultSchema = z.object({ attachmentId: z.number().int(), status: z.literal("uploaded") });
export const attachmentPageSchema = z.object({ page: z.number().int(), text: z.string(), offset: z.number().int(), nextOffset: z.number().int(), eof: z.boolean() });

export const principalSchema = z.object({
  id: z.string(), name: z.string(), iss: z.string(), sub: z.string(), label: z.string().nullable().optional(),
  scopes: z.array(z.string()), builtin: z.boolean(), maskedByBuiltin: z.boolean(),
});
export const principalsSchema = z.object({ principals: z.array(principalSchema), defaultNewPrincipalScopes: z.array(z.string()) });
export type Principal = z.infer<typeof principalSchema>;
export const waitlistSchema = z.object({ entries: z.array(z.object({
  id: z.string(), name: z.string(), iss: z.string(), sub: z.string(), firstSeenUs: z.number().int(), lastSeenUs: z.number().int(),
})) });
export const webhookInputSchema = z.object({
  endpoint: z.string(), consumerOrigin: z.string(), secretRef: z.string(),
  eventOperations: z.array(z.enum(["create", "update", "delete", "rename"])),
  entityTypes: z.array(z.string()), ignoredOrigins: z.array(z.string()), enabled: z.boolean().optional(),
});
export type WebhookInput = z.infer<typeof webhookInputSchema>;
export const webhookSchema = webhookInputSchema.extend({ subscriptionId: z.string(), enabled: z.boolean() });
export const webhooksSchema = z.object({ subscriptions: z.array(webhookSchema), configuredSecrets: z.array(z.string()), deliveryRole: z.boolean() });
export const webhookTestSchema = z.object({ status: z.number().int(), ok: z.boolean(), latencyUs: z.number().nonnegative() });
export const repoInputSchema = z.object({ key: z.string(), url: z.string(), authKind: z.enum(["none", "token", "ssh"]), authSecret: z.string().optional(), snippets: z.boolean() });
export type RepoInput = z.infer<typeof repoInputSchema>;
export const repoSchema = repoInputSchema.omit({ authSecret: true }).extend({
  state: z.enum(["pending", "cloning", "indexing", "indexed", "error", "removing"]),
  lastError: z.string().nullable(), lastIndexedUs: z.number().int().nullable(),
});
export const reposSchema = z.object({ repos: z.array(repoSchema) });
export const acceptedSchema = z.object({ status: z.literal("accepted"), key: z.string() });
export const vectorStatsSchema = z.object({ embeddingCount: z.number().int(), dims: z.number().int(), petgraphNodes: z.number().int(), petgraphEdges: z.number().int() });
