import type { Tenant } from "agentdb";

import { errorResponse, tenantFrom } from "@/lib/db";
import { loadSample } from "@/lib/sample";

export const dynamic = "force-dynamic";

/* eslint-disable @typescript-eslint/no-explicit-any */
const actions: Record<string, (db: Tenant, args: any) => Promise<unknown>> = {
  describe: (db) => db.describe(),
  find: (db, { query }) => db.find(query),
  insert: (db, { table, doc }) => db.insert(table, doc),
  update: (db, { table, id, patch, version }) => db.update(table, id, patch, { version }),
  delete: (db, { table, id }) => db.delete(table, id).then(() => ({ deleted: true })),
  ask: (db, { text }) => db.ask(text),
  migrate: (db, { changes }) => db.migrate(changes),
  sample: (db) => loadSample(db),
};

/** One endpoint for every database call the page makes: `{ action, ...args }`. */
export async function POST(request: Request) {
  try {
    const { action, ...args } = await request.json();
    const run = actions[action];
    if (!run) return Response.json({ error: { code: "unknown_action", message: `unknown action \`${action}\`` } }, { status: 400 });
    return Response.json(await run(tenantFrom(request), args));
  } catch (error) {
    return errorResponse(error);
  }
}
