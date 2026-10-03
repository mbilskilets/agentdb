import { AgentDB, AgentDBError, type Tenant } from "agentdb";

let client: AgentDB | undefined;

/** The tenant a request is about, from its `tenant` query parameter. */
export function tenantFrom(request: Request): Tenant {
  client ??= new AgentDB({ url: process.env.AGENTDB_URL, secret: process.env.AGENTDB_SECRET });
  const id = new URL(request.url).searchParams.get("tenant") || "demo";
  return client.tenant(id);
}

/** Turns a thrown error into the JSON the browser shows. */
export function errorResponse(error: unknown): Response {
  if (error instanceof AgentDBError) {
    return Response.json({ error: { code: error.code, message: error.message } }, { status: error.status || 502 });
  }
  const message = error instanceof Error ? error.message : String(error);
  return Response.json({ error: { code: "internal", message } }, { status: 500 });
}
