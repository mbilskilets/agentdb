import { AgentDB, AgentDBError, type Tenant } from "agentdb";
import type { NextRequest } from "next/server";

let client: AgentDB | undefined;

/** The tenant a request is about, from its `tenant` query parameter. */
export function tenantFrom(request: NextRequest): Tenant {
  client ??= new AgentDB({ url: process.env.AGENTDB_URL, secret: process.env.AGENTDB_SECRET });
  return client.tenant(request.nextUrl.searchParams.get("tenant") || "demo");
}

/** Turns a thrown error into the JSON the browser shows. */
export function errorResponse(error: unknown): Response {
  if (error instanceof AgentDBError) {
    return Response.json({ error: { code: error.code, message: error.message } }, { status: error.status || 502 });
  }
  const message = error instanceof Error ? error.message : String(error);
  return Response.json({ error: { code: "internal", message } }, { status: 500 });
}
