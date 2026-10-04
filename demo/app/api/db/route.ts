import type { Tenant } from "agentdb";
import type { NextRequest } from "next/server";
import { z } from "zod";

import { refusal } from "@/lib/access";
import { errorResponse, tenantFrom } from "@/lib/db";
import { loadSample } from "@/lib/sample";

export const dynamic = "force-dynamic";

const MAX_ASK_CHARS = 500;
const table = z.string();
const id = z.number().int();
const version = z.number().int().optional();
const fields = z.record(z.string(), z.unknown());

/** Every database call the page makes. Nothing else gets through, and the page never changes a schema. */
const Call = z.discriminatedUnion("action", [
  z.object({ action: z.literal("describe") }),
  z.object({ action: z.literal("find"), table, limit: z.number().int().positive() }),
  z.object({ action: z.literal("insert"), table, doc: fields }),
  z.object({ action: z.literal("update"), table, id, patch: fields, version }),
  z.object({ action: z.literal("delete"), table, id, version }),
  z.object({ action: z.literal("ask"), text: z.string().min(1).max(MAX_ASK_CHARS) }),
  z.object({ action: z.literal("sample") }),
]);

function run(db: Tenant, call: z.infer<typeof Call>): Promise<unknown> {
  switch (call.action) {
    case "describe":
      return db.describe();
    case "find":
      return db.find({ table: call.table, limit: call.limit });
    case "insert":
      return db.insert(call.table, call.doc);
    case "update":
      return db.update(call.table, call.id, call.patch, { version: call.version });
    case "delete":
      return db.delete(call.table, call.id, { version: call.version }).then(() => ({ deleted: true }));
    case "ask":
      return db.ask(call.text);
    case "sample":
      return loadSample(db);
  }
}

/** One endpoint for every database call the page makes: `{ action, ...args }`. */
export async function POST(request: NextRequest) {
  const refused = refusal(request);
  if (refused) return refused;
  const call = Call.safeParse(await request.json().catch(() => undefined));
  if (!call.success) {
    return Response.json({ error: { code: "invalid_request", message: `the demo does not make this call: ${z.prettifyError(call.error)}` } }, { status: 400 });
  }
  try {
    return Response.json(await run(tenantFrom(request), call.data));
  } catch (error) {
    return errorResponse(error);
  }
}
