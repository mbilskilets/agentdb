import { afterAll, beforeAll, describe, expect, test } from "vitest";

import { AgentDB, AgentDBError, type Change, type Query, type Tenant, type Write } from "../src/index.js";
import { SECRET, startServer, typesafeKey } from "./server.js";

interface Client {
  name: string;
  email?: string;
  status?: "lead" | "active";
  revenue?: number;
}

let agentdb: AgentDB;
let stop: () => void;
let counter = 0;

beforeAll(async () => {
  const server = await startServer();
  stop = server.stop;
  agentdb = new AgentDB({ url: server.url, secret: SECRET });
});
afterAll(() => stop());

/** A fresh tenant with a `clients` table. */
async function crm(): Promise<Tenant> {
  const db = agentdb.tenant(`tenant_${++counter}`);
  await db.migrate([
    {
      op: "define_table",
      table: {
        name: "clients",
        description: "Businesses we sell to",
        fields: [
          { name: "name", type: "text", required: true },
          { name: "email", type: "text", required: false },
          { name: "status", type: "enum", values: ["lead", "active"], required: false },
          { name: "revenue", type: "number", required: false },
        ],
      },
    },
  ]);
  return db;
}

/** Adds `count` clients, named `Client 0` and up, with their number as revenue. */
async function insertClients(db: Tenant, count: number): Promise<void> {
  const writes: Write[] = Array.from({ length: count }, (_, n) => ({ op: "insert", table: "clients", doc: { name: `Client ${n}`, revenue: n } }));
  for (let first = 0; first < count; first += 500) await db.batch(writes.slice(first, first + 500));
}

async function failure(work: Promise<unknown>): Promise<AgentDBError> {
  const error = await work.then(
    () => undefined,
    (thrown: unknown) => thrown,
  );
  expect(error).toBeInstanceOf(AgentDBError);
  return error as AgentDBError;
}

async function until(done: () => boolean): Promise<void> {
  for (let attempt = 0; attempt < 100 && !done(); attempt++) await new Promise((resolve) => setTimeout(resolve, 20));
  expect(done()).toBe(true);
}

describe("documents", () => {
  test("insert, get, update, find and delete round-trip", async () => {
    const db = await crm();
    const acme = await db.insert<Client>("clients", { name: "Acme", status: "lead", revenue: 900 });
    expect(acme).toMatchObject({ id: 1, version: 1, name: "Acme", status: "lead" });
    expect(await db.get<Client>("clients", acme.id)).toEqual(acme);

    const updated = await db.update<Client>("clients", acme.id, { status: "active", revenue: null }, { version: acme.version });
    expect(updated.version).toBe(2);
    expect(updated.status).toBe("active");
    expect(updated.revenue).toBeUndefined();

    await db.insert<Client>("clients", { name: "Globex", revenue: 5000 });
    const page = await db.find<Client>({ table: "clients", where: [{ field: "revenue", op: "gt", value: 1000 }], sort: { field: "revenue", descending: true }, limit: 10 });
    expect(page.total).toBe(1);
    expect(page.docs[0]?.name).toBe("Globex");
    expect(page.next_offset).toBeNull();

    await db.delete("clients", acme.id);
    expect((await failure(db.get("clients", acme.id))).code).toBe("not_found");
  });

  test("a delete with a stale version is refused", async () => {
    const db = await crm();
    const acme = await db.insert<Client>("clients", { name: "Acme" });
    await db.update<Client>("clients", acme.id, { revenue: 1 });
    const stale = await failure(db.delete("clients", acme.id, { version: acme.version }));
    expect([stale.code, stale.status]).toEqual(["version_conflict", 409]);
    await db.delete("clients", acme.id, { version: 2 });
    expect((await db.find({ table: "clients" })).total).toBe(0);
  });

  test("a batch applies every write or none", async () => {
    const db = await crm();
    const docs = await db.batch<Client>([
      { op: "insert", table: "clients", doc: { name: "Acme" } },
      { op: "insert", table: "clients", doc: { name: "Globex" } },
      { op: "update", table: "clients", id: 1, patch: { revenue: 5 }, version: 1 },
      { op: "delete", table: "clients", id: 2 },
    ]);
    expect(docs.map((doc) => [doc.id, doc.version, doc.name, doc.revenue])).toEqual([
      [1, 1, "Acme", undefined],
      [2, 1, "Globex", undefined],
      [1, 2, "Acme", 5],
      [2, 1, "Globex", undefined],
    ]);

    const error = await failure(
      db.batch([
        { op: "insert", table: "clients", doc: { name: "Initech" } },
        { op: "insert", table: "clients", doc: { name: "Hooli", emial: "x" } },
      ]),
    );
    expect(error.code).toBe("unknown_field");
    expect(error.message).toMatch(/^step 2 of 2 failed, so none of the 2 changes were applied/);
    expect((await db.find({ table: "clients" })).total).toBe(1);
  });

  test("describe reports tables, fields and counts", async () => {
    const db = await crm();
    await db.insert("clients", { name: "Acme" });
    const [clients] = await db.describe();
    expect(clients).toMatchObject({ name: "clients", description: "Businesses we sell to", count: 1 });
    expect(clients?.fields.map((field) => field.name)).toEqual(["name", "email", "status", "revenue"]);
  });

  test("tenants do not see each other", async () => {
    const db = await crm();
    await db.insert("clients", { name: "Acme" });
    expect(await agentdb.tenant(`empty_${++counter}`).describe()).toEqual([]);
  });
});

describe("indexes", () => {
  test("a unique field refuses a second document with the same value", async () => {
    const db = await crm();
    const [clients] = await db.migrate([{ op: "set_unique", table: "clients", field: "email", unique: true }]);
    expect(clients?.fields[1]).toEqual({ name: "email", type: "text", required: false, indexed: true, unique: true });
    await db.insert("clients", { name: "Acme", email: "hello@acme.io" });
    const duplicate = await failure(db.insert("clients", { name: "Acme again", email: "hello@acme.io" }));
    expect([duplicate.code, duplicate.status]).toEqual(["duplicate_value", 409]);
  });

  test("a table over 1,000 documents is only searched through an index", async () => {
    const db = await crm();
    await insertClients(db, 1001);
    const byRevenue: Query = { table: "clients", where: [{ field: "revenue", op: "gte", value: 1000 }] };
    const refused = await failure(db.find(byRevenue));
    expect(refused.code).toBe("query_needs_index");
    expect(refused.message).toContain('{"op": "set_indexed", "table": "clients", "field": "revenue", "indexed": true}');

    const [clients] = await db.migrate([{ op: "set_indexed", table: "clients", field: "revenue", indexed: true }]);
    expect(clients?.fields[3]).toEqual({ name: "revenue", type: "number", required: false, indexed: true });
    expect((await db.find<Client>(byRevenue)).docs.map((doc) => doc.name)).toEqual(["Client 1000"]);
  });
});

describe("errors", () => {
  test("carry a code, a status and the teaching message", async () => {
    const db = await crm();
    const typo = await failure(db.insert("clients", { name: "Acme", emial: "a@acme.io" }));
    expect(typo.code).toBe("unknown_field");
    expect(typo.status).toBe(400);
    expect(typo.message).toBe("unknown field `emial` on table `clients`. Did you mean `email`? Valid fields: name, email, status, revenue.");

    await db.insert("clients", { name: "Acme" });
    await db.update("clients", 1, { revenue: 1 }, { version: 1 });
    const stale = await failure(db.update("clients", 1, { revenue: 2 }, { version: 1 }));
    expect([stale.code, stale.status]).toEqual(["version_conflict", 409]);
  });

  test("a failed migration changes nothing and names the step", async () => {
    const db = await crm();
    const error = await failure(
      db.migrate([
        { op: "rename_field", table: "clients", field: "email", new_name: "mail" },
        { op: "add_enum_value", table: "clients", field: "name", value: "x" },
      ]),
    );
    expect(error.code).toBe("not_an_enum");
    expect(error.message).toMatch(/^step 2 of 2 failed, so none of the 2 changes were applied/);
    expect((await db.describe())[0]?.fields.map((field) => field.name)).toContain("email");
  });

  test("an unreachable server and missing settings are explained", async () => {
    const offline = new AgentDB({ url: "http://127.0.0.1:9", secret: "x" }).tenant("t");
    const error = await failure(offline.describe());
    expect(error.code).toBe("unreachable");
    expect(error.message).toContain("Is the server running?");
    expect(() => new AgentDB({ url: undefined, secret: "x" })).toThrow(/needs the server `url`/);
    const wrong = new AgentDB({ url: agentdb.url, secret: "wrong" }).tenant("t");
    expect((await failure(wrong.describe())).code).toBe("unauthorized");
  });
});

describe("subscribe", () => {
  test("delivers only new changes by default, in order", async () => {
    const db = await crm();
    await db.insert("clients", { name: "Before" });
    const seen: Change[] = [];
    const stopWatching = db.subscribe((change) => seen.push(change));
    await new Promise((resolve) => setTimeout(resolve, 150));
    await db.insert("clients", { name: "Acme" });
    await db.update("clients", 2, { revenue: 5 });
    await db.migrate([{ op: "add_field", table: "clients", field: { name: "phone", type: "text", required: false } }]);
    await db.delete("clients", 2);
    await until(() => seen.length === 4);
    stopWatching();
    expect(seen.map((change) => change.kind)).toEqual(["insert", "update", "schema", "delete"]);
    expect(seen.map((change) => change.seq)).toEqual([3, 4, 5, 6]);
    expect(seen[2]?.doc).toBeNull();
    await db.insert("clients", { name: "After stopping" });
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(seen.length).toBe(4);
  });

  test("replays from `since`", async () => {
    const db = await crm();
    await db.insert("clients", { name: "Acme" });
    const seen: Change[] = [];
    const stopWatching = db.subscribe((change) => seen.push(change), { since: 0 });
    await until(() => seen.length === 2);
    stopWatching();
    expect(seen.map((change) => change.kind)).toEqual(["schema", "insert"]);
    expect(await db.changes(1)).toEqual({ changes: [seen[1]], latest_seq: 2 });
    expect(await db.changes()).toEqual({ changes: [], latest_seq: 2 });
  });

  test("a subscriber further behind than the log reaches is told, then carries on", async () => {
    const db = await crm();
    await insertClients(db, 10_000);
    const events: string[] = [];
    const stopWatching = db.subscribe((change) => events.push(`change ${change.seq}`), {
      since: 0,
      onGap: () => events.push("gap"),
      onError: (error) => events.push(`error ${String(error)}`),
    });
    await until(() => events.length === 1);
    await new Promise((resolve) => setTimeout(resolve, 150));
    await db.insert("clients", { name: "Acme" });
    await until(() => events.length === 2);
    stopWatching();
    expect(events).toEqual(["gap", "change 10002"]);
  });

  test("subscribing to a long change log starts with the next write", async () => {
    const db = await crm();
    await insertClients(db, 10_000);
    const events: string[] = [];
    const stopWatching = db.subscribe((change) => events.push(`change ${change.seq}`), {
      onGap: () => events.push("gap"),
      onError: (error) => events.push(`error ${String(error)}`),
    });
    await new Promise((resolve) => setTimeout(resolve, 150));
    await db.insert("clients", { name: "Acme" });
    await until(() => events.length === 1);
    stopWatching();
    expect(events).toEqual(["change 10002"]);
  });

  test("a feed the server ends for falling behind is a gap too", async () => {
    const insert = (seq: number) => `id: ${seq}\ndata: ${JSON.stringify({ seq, table: "clients", kind: "insert", at: "2026-10-04T00:00:00Z", doc: null })}\n\n`;
    const trimmed = `event: error\ndata: ${JSON.stringify({ error: { code: "changes_trimmed", message: "cannot replay changes after seq 3" } })}\n\n`;
    const feeds = [insert(3) + trimmed, insert(9)];
    const requests: string[] = [];
    const server: typeof fetch = async (url) => {
      const path = String(url).replace("http://agentdb.test/v1/tenants/acme", "");
      requests.push(path);
      return path === "/changes" ? Response.json({ changes: [], latest_seq: 8 }) : new Response(feeds.shift() ?? "");
    };
    const db = new AgentDB({ url: "http://agentdb.test", secret: SECRET, fetch: server }).tenant("acme");
    const events: string[] = [];
    const stopWatching = db.subscribe((change) => events.push(`change ${change.seq}`), {
      since: 2,
      onGap: () => events.push("gap"),
      onError: (error) => events.push(`error ${String(error)}`),
    });
    await until(() => events.length === 3);
    stopWatching();
    expect(events).toEqual(["change 3", "gap", "change 9"]);
    expect(requests.slice(0, 3)).toEqual(["/subscribe?since=2", "/changes", "/subscribe?since=8"]);
  });
});

describe("tools", () => {
  const call = (tool: unknown, input: unknown) => (tool as { execute: (input: unknown, options: unknown) => Promise<any> }).execute(input, { toolCallId: "test", messages: [] });
  const schemaOf = async (tool: unknown) => JSON.stringify(await (tool as { inputSchema: { jsonSchema: unknown } }).inputSchema.jsonSchema);

  test("input schemas list the tenant's exact fields and allowed values", async () => {
    const db = await crm();
    const tools = await db.tools();
    expect(Object.keys(tools).sort()).toEqual(["ask_database", "change_schema", "delete_document", "describe_database", "find_documents", "get_document", "insert_document", "update_document", "write_documents"]);
    const insert = JSON.parse(await schemaOf(tools.insert_document));
    const clients = insert.properties.document.anyOf[0];
    expect(clients.title).toBe("clients");
    expect(clients.required).toEqual(["name"]);
    expect(clients.properties.status).toEqual({ type: "string", enum: ["lead", "active"] });
    expect(clients.additionalProperties).toBe(false);
    expect(tools.find_documents?.description).toContain("- clients (Businesses we sell to): name: text, email?: text, status?: lead|active, revenue?: number");

    const readOnly = await db.tools({ schemaChanges: false, deletes: false, ask: false });
    expect(Object.keys(readOnly).sort()).toEqual(["describe_database", "find_documents", "get_document", "insert_document", "update_document", "write_documents"]);
  });

  test("the model is told which fields are indexed and what large tables need", async () => {
    const db = await crm();
    await db.migrate([
      { op: "set_unique", table: "clients", field: "email", unique: true },
      { op: "set_indexed", table: "clients", field: "status", indexed: true },
    ]);
    const find = (await db.tools()).find_documents?.description;
    expect(find).toContain("- clients (Businesses we sell to): name: text, email?: text (unique), status?: lead|active (indexed), revenue?: number");
    expect(find).toContain("A table with more than 1,000 documents is searched only through an index");
    expect(find).toContain('index it first with change_schema: {"op": "set_indexed"');
    const withoutSchemaChanges = (await db.tools({ schemaChanges: false })).find_documents?.description;
    expect(withoutSchemaChanges).toContain("A table with more than 1,000 documents is searched only through an index");
    expect(withoutSchemaChanges).not.toContain("change_schema");
  });

  test("the agent can index a field, make one unique and delete with a version", async () => {
    const db = await crm();
    const tools = await db.tools();
    const schema = await call(tools.change_schema, {
      changes: [
        { op: "set_indexed", table: "clients", field: "revenue", indexed: true },
        { op: "set_unique", table: "clients", field: "email", unique: true },
        { op: "add_field", table: "clients", field: { name: "phone", type: "text", required: false, indexed: true } },
      ],
    });
    expect(schema[0].fields.filter((field: { indexed?: boolean }) => field.indexed).map((field: { name: string }) => field.name)).toEqual(["email", "revenue", "phone"]);

    await call(tools.insert_document, { table: "clients", document: { name: "Acme" } });
    await call(tools.update_document, { table: "clients", id: 1, patch: { revenue: 5 } });
    const stale = await call(tools.delete_document, { table: "clients", id: 1, version: 1 });
    expect(stale.code).toBe("version_conflict");
    expect(await call(tools.delete_document, { table: "clients", id: 1, version: 2 })).toEqual({ deleted: true });
  });

  test("write_documents applies a batch as a unit and names the write that failed", async () => {
    const db = await crm();
    const tools = await db.tools();
    const written = await call(tools.write_documents, {
      writes: [
        { op: "insert", table: "clients", doc: { name: "Acme" } },
        { op: "insert", table: "clients", doc: { name: "Globex" } },
        { op: "update", table: "clients", id: 1, patch: { status: "active" } },
        { op: "delete", table: "clients", id: 2 },
      ],
    });
    expect(written.map((doc: { id: number; version: number }) => [doc.id, doc.version])).toEqual([[1, 1], [2, 1], [1, 2], [2, 1]]);
    const failed = await call(tools.write_documents, {
      writes: [
        { op: "insert", table: "clients", doc: { name: "Initech" } },
        { op: "insert", table: "clients", doc: { name: "Hooli", stauts: "lead" } },
      ],
    });
    expect(failed.code).toBe("unknown_field");
    expect(failed.error).toMatch(/^step 2 of 2 failed, so none of the 2 changes were applied: unknown field `stauts`/);
    expect((await db.find({ table: "clients" })).total).toBe(1);
  });

  test("a batch cannot delete when deletes are turned off", async () => {
    const db = await crm();
    await db.insert("clients", { name: "Acme" });
    const tools = await db.tools({ deletes: false });
    const ops = JSON.parse(await schemaOf(tools.write_documents)).properties.writes.items.anyOf.map((write: { properties: { op: { enum: string[] } } }) => write.properties.op.enum[0]);
    expect(ops).toEqual(["insert", "update"]);
    expect(tools.write_documents?.description).toContain("inserts and updates, in order");

    const refused = await call(tools.write_documents, {
      writes: [
        { op: "insert", table: "clients", doc: { name: "Globex" } },
        { op: "delete", table: "clients", id: 1 },
      ],
    });
    expect(refused).toEqual({ code: "deletes_disabled", error: "this batch contains a delete, and you may not delete documents. Nothing was written. Send it again without the delete." });
    expect((await db.find<Client>({ table: "clients" })).docs.map((doc) => doc.name)).toEqual(["Acme"]);
  });

  test("run against the database and hand errors back to the model", async () => {
    const db = await crm();
    const tools = await db.tools();
    const inserted = await call(tools.insert_document, { table: "clients", document: { name: "Acme", status: "lead" } });
    expect(inserted).toMatchObject({ id: 1, name: "Acme" });
    const typo = await call(tools.insert_document, { table: "clients", document: { name: "Globex", stauts: "lead" } });
    expect(typo).toEqual({ code: "unknown_field", error: "unknown field `stauts` on table `clients`. Did you mean `status`? Valid fields: name, email, status, revenue." });
    const updated = await call(tools.update_document, { table: "clients", id: 1, patch: { status: "active" }, version: 1 });
    expect(updated.version).toBe(2);
    const found = await call(tools.find_documents, { table: "clients", where: [{ field: "status", op: "eq", value: "active" }] });
    expect(found.total).toBe(1);
    expect(await call(tools.delete_document, { table: "clients", id: 1 })).toEqual({ deleted: true });
  });

  test("a table created by the agent is usable in the same conversation", async () => {
    const db = await crm();
    const tools = await db.tools();
    const schema = await call(tools.change_schema, {
      changes: [{ op: "define_table", table: { name: "products", fields: [{ name: "title", type: "text", required: true }, { name: "price", type: "number", required: false }] } }],
    });
    expect(schema.map((table: { name: string }) => table.name)).toEqual(["clients", "products"]);
    const product = await call(tools.insert_document, { table: "products", document: { title: "Widget", price: 9.5 } });
    expect(product).toMatchObject({ id: 1, title: "Widget" });
    const refused = await call(tools.change_schema, { changes: [{ op: "drop_table", table: "products" }] });
    expect(refused.code).toBe("would_destroy");
    expect(await schemaOf((await db.tools()).insert_document)).toContain('"title":"products"');
  });
});

describe("ask", () => {
  test.runIf(typesafeKey())("answers an English request and shows how it was understood", async () => {
    const db = await crm();
    await db.insert<Client>("clients", { name: "Acme", status: "lead" });
    await db.insert<Client>("clients", { name: "Globex", status: "active" });
    const asked = await db.ask<Client>("active clients");
    expect(asked.query).toMatchObject({ table: "clients", where: [{ field: "status", op: "eq", value: "active" }] });
    expect(asked.page?.docs.map((doc) => doc.name)).toEqual(["Globex"]);
    const refused = await db.ask("delete all clients");
    expect(refused.page).toBeNull();
    expect(refused.refusal).toMatch(/^ask\(\) only reads/);
  });
});
