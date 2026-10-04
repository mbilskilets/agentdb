# agentdb

A small document database where every caller is an AI agent.

Agents use a database differently than people do. They misspell field names. They invent columns that sound right. Two of them overwrite the same record a second apart. And when a call fails, the error text is all they have to go on, because nobody is there to open a debugger.

Most databases answer those mistakes with errors written for a human. agentdb writes them for the agent:

```
unknown field `emial` on table `clients`. Did you mean `email`?
Valid fields: name, email, status, revenue.
```

```
`clients` id 1 is at version 2, but this write expected version 1.
Someone else changed it: read it again with get() and retry with version 2.
```

```
this would permanently delete table `clients` and everything in it,
affecting 42 document(s). Call again with force = true if that is intended.
```

Each message says what was wrong and what a correct call looks like. In the demo, an agent that makes a typo reads the error, fixes its own call and carries on.

## What you get

- **One encrypted file per tenant.** Each tenant is a separate SQLCipher database. The server derives its key from one master secret, so your app never handles keys.
- **Documents with a checked schema.** Tables hold JSON documents. Every write is checked against the table's fields, types, allowed values and links to other tables. Nothing malformed gets stored.
- **Tables that agents can change.** An agent can create a table, add a field, rename one or change its type while the app runs. Several changes apply as one unit, or not at all.
- **Indexes and unique fields.** A table of any size can be searched by a field you mark `indexed`. A field you mark `unique` never holds the same value twice.
- **Writes that land together.** `batch` applies up to 500 writes as one unit, about 40 times faster per document than writing them one by one.
- **A live change feed.** Every write gets a sequence number. Your UI subscribes and sees rows change as agents work.
- **Queries in English.** `ask("clients created today")` returns the documents and the exact query it ran. When it is unsure, it refuses and says why.
- **Tools for your agents.** `db.tools()` returns ready-made tools for the Vercel AI SDK, built from the tenant's current schema.

It is meant for the tables a CRM would have, such as clients, employees and deals. It is not a replacement for Postgres or Convex in a large app. See [limits](#limits) before you rely on it.

## Quick start

You need Rust and Node 22.

```bash
git clone https://github.com/mbilskilets/agentdb.git && cd agentdb

cat > .env <<'EOF'
AGENTDB_SECRET=<a random string, at least 32 characters>
AGENTDB_MASTER_KEY=<another one>
TYPESAFE_API_KEY=<optional, for English queries>
OPENROUTER_API_KEY=<optional, for the demo's agent chat>
DEMO_PASSWORD=<optional, at least 12 characters, to log in to the demo>
EOF

./demo.sh
```

`openssl rand -hex 32` prints a good secret. The server refuses to start with one shorter than 32 characters, or with the same value for both.

`demo.sh` builds everything, starts the server on port 4000 and the demo app on port 3000. Open http://localhost:3000, log in with the password `demo.sh` prints, and click "Load sample data".

To run only the server:

```bash
cargo build --release --bin agentdb-server
set -a; . ./.env; set +a
./target/release/agentdb-server
```

| Variable | What it does |
|---|---|
| `AGENTDB_SECRET` | Clients send it as `Authorization: Bearer <secret>`. Required. |
| `AGENTDB_MASTER_KEY` | Every tenant's encryption key comes from it. Required. If you lose it, no tenant file can be read again, so back it up. |
| `AGENTDB_DATA_DIR` | Where tenant files go. Defaults to `./data`. |
| `AGENTDB_HOST`, `AGENTDB_PORT` | Defaults to `127.0.0.1` and `4000`. |
| `AGENTDB_MAX_OPEN_TENANTS` | How many tenant databases stay open at once. Defaults to 100. See [running the server](#running-the-server). |
| `TYPESAFE_API_KEY` | Turns on `ask`. Everything else works without it. |

## TypeScript SDK

The SDK lives in `sdk/`. It is not on npm yet, so build it and install it from the folder:

```bash
cd sdk && npm install && npm run build
cd ../your-app && npm install ../agentdb/sdk
```

It has no runtime dependencies. `db.tools()` needs the `ai` package, and the tests run against version 7.

### Connect

```ts
import { AgentDB } from "agentdb";

const agentdb = new AgentDB({
  url: process.env.AGENTDB_URL,       // http://127.0.0.1:4000
  secret: process.env.AGENTDB_SECRET,
});

const db = agentdb.tenant("org_42");
```

`tenant()` costs nothing. The server creates the tenant's file when you define its first table. Tenant ids may contain lowercase letters, digits, `_` and `-`.

Keep the SDK on your backend. Anyone holding the secret can read every tenant.

### Define tables

```ts
await db.migrate([
  {
    op: "define_table",
    table: {
      name: "companies",
      fields: [{ name: "name", type: "text", required: true }],
    },
  },
  {
    op: "define_table",
    table: {
      name: "clients",
      description: "Customers and prospects",
      fields: [
        { name: "name", type: "text", required: true },
        { name: "email", type: "text", required: false, unique: true },
        { name: "status", type: "enum", values: ["lead", "active", "churned"], required: false, indexed: true },
        { name: "revenue", type: "number", required: false },
        { name: "vip", type: "bool", required: false },
        { name: "signed_at", type: "datetime", required: false },
        { name: "company", type: "ref", table: "companies", required: false },
      ],
    },
  },
]);
```

| Field type | Holds |
|---|---|
| `text` | A string |
| `number` | An integer or a decimal |
| `bool` | `true` or `false` |
| `datetime` | An RFC 3339 timestamp or a `YYYY-MM-DD` date. Stored in UTC. |
| `enum` | One of the listed `values` |
| `ref` | The id of a document in another `table`. The database refuses an id that does not exist. |

The database adds `id`, `version`, `created_at` and `updated_at` to every document.

Two more properties change how a field behaves.

`indexed: true` lets a table of any size be filtered and sorted by the field. You need it once a table grows past 1,000 documents, as [large tables](#large-tables) explains. A table can have 10 indexed fields, because every index makes each write a little slower.

`unique: true` refuses a second document with the same value:

```
`email` must be unique on `clients`, and `clients` id 1 already holds "hello@acme.com".
Update that document instead of adding another one, or use a different value.
```

Documents that leave the field unset do not clash. A unique field is always indexed, and so is a `ref`.

### Change tables later

`migrate` takes a list of changes and applies them in order. If step 3 fails, steps 1 and 2 are undone and the error names the step.

```ts
await db.migrate([
  { op: "rename_field", table: "clients", field: "email", new_name: "contact_email" },
  { op: "add_field", table: "clients", field: { name: "phone", type: "text", required: false } },
  { op: "change_type", table: "clients", field: "revenue", to: { type: "text" } },
]);
```

| Change | What it does |
|---|---|
| `define_table` | Creates a table |
| `add_field` | Adds a field. A required field needs an empty table. |
| `rename_table`, `rename_field` | Renames it and moves the data. Links from other tables follow. |
| `change_type` | Converts every stored value. If one value does not fit, nothing changes and the error shows an example. |
| `set_required` | Makes a field required or optional. Refuses while documents lack a value. |
| `set_indexed` | Adds or removes a field's index. Refuses an eleventh index on a table, and refuses to remove the index of a unique field or a `ref`. |
| `set_unique` | Makes a field unique, or lifts that. Refuses while two documents hold the same value, and names two of them. |
| `add_enum_value`, `remove_enum_value` | Edits the allowed values. Removal refuses while documents use the value. |
| `describe` | Sets a plain-words description on a table or field |
| `remove_field`, `drop_table` | Deletes data, so both refuse and report the damage unless you pass `force: true` |

`db.describe()` returns every table with its fields and document count, and shows which fields are indexed or unique. Call it first when an agent needs to find its way around.

### Read and write documents

```ts
const acme = await db.insert("clients", { name: "Acme", status: "lead" });
// { id: 1, version: 1, created_at: "...", updated_at: "...", name: "Acme", status: "lead" }

await db.get("clients", 1);

// Changes only the fields you pass. null removes an optional field.
await db.update("clients", 1, { status: "active", email: null }, { version: acme.version });

await db.delete("clients", 1, { version: 2 });
```

Pass `version` on updates and deletes. If another agent changed the document since you read it, the write fails with `version_conflict` instead of overwriting or deleting their work.

A delete fails while other documents link to the one you are deleting. The error counts them.

### Write many documents at once

```ts
const docs = await db.batch([
  { op: "insert", table: "clients", doc: { name: "Globex", status: "lead" } },
  { op: "insert", table: "clients", doc: { name: "Initech" } },
  { op: "update", table: "clients", id: 1, patch: { status: "active" }, version: 1 },
  { op: "delete", table: "clients", id: 7 },
]);
```

`batch` takes up to 500 writes and applies them in order as one unit. If the third write fails, the first two are undone and the error names the step, as `migrate` does. A later write sees the earlier ones. You get one document back per write: the document after the write, or its last state for a delete.

Use it whenever you write more than one document. A single write is saved to disk on its own and takes about a millisecond. A batch is saved once, and that brings a document down to 0.025 ms.

### Query

```ts
const page = await db.find({
  table: "clients",
  where: [
    { field: "status", op: "eq", value: "active" },
    { field: "revenue", op: "gt", value: 5000 },
  ],
  sort: { field: "revenue", descending: true },
  limit: 10,
});

page.docs;         // the documents
page.total;        // how many match across all pages
page.next_offset;  // pass as `offset` for the next page, or null at the end
```

All filters must match. The operators are `eq`, `ne`, `gt`, `gte`, `lt`, `lte` and `contains`, a case-insensitive text search. Use `value: null` with `eq` to find documents where a field is unset. You can filter and sort on `id`, `created_at` and `updated_at` too. Documents with the same sort value come in `id` order, reversed when the sort is descending.

There is no OR and no filtering across tables yet.

### Large tables

A table with more than 1,000 documents is only searched through an index. That way no query can stall a tenant by reading every document it has. On such a table a query needs one of these:

- a filter with `eq`, `gt`, `gte`, `lt` or `lte` on an indexed field
- no filter at all, and either no sort or a sort by an indexed field

`id`, `created_at` and `updated_at` are always indexed. `ne` and `contains` never use an index, but they work next to a filter that does.

Any other query is refused with `query_needs_index`. This is what the agent reads:

```
cannot run this query: `clients` holds 1500 documents, and a table with more than 1000 is
only searched through an index. The filter on `revenue` has no index to use. Indexed fields:
id, created_at, updated_at, email, status. Add a filter on one of them that leaves few
documents to read (an `eq`, or a narrow `gt`, `gte`, `lt` or `lte` range), or index `revenue`
first with the schema change {"op": "set_indexed", "table": "clients", "field": "revenue",
"indexed": true}.
```

An index does not make every query fast. A range that covers most of the table, a `contains` over many documents and a large `offset` still read a lot. A read that runs longer than two seconds is stopped with `query_too_slow`, and that message says what to narrow.

### Ask in English

```ts
const asked = await db.ask("vip clients with revenue over 10000");

if (asked.page) {
  asked.page.docs;   // the results
  asked.query;       // the exact query that ran
} else {
  asked.refusal;     // why it did not run
  asked.query;       // its best guess, or null
}
```

`ask` only reads. Always check `asked.query`, because that is what ran. If the query is right but `ask` refused out of caution, pass it to `find`.

When the table is too large for the query `ask` built, `refusal` carries the `query_needs_index` message and `query` still shows what it understood.

See [how English queries work](#how-english-queries-work) for what it can do and how often it gets it right.

### Subscribe to changes

```ts
const stop = db.subscribe(
  (change) => {
    change.seq;    // grows by one per write
    change.kind;   // "insert" | "update" | "delete" | "schema"
    change.table;
    change.doc;    // the document after the write, or null for a schema change
  },
  { onGap: () => loadEverythingAgain() },
);

stop();
```

By default you get changes made after you subscribe. Pass `{ since: seq }` to replay everything after a known point first. The connection reconnects by itself and resumes where it left off.

The server keeps the newest 10,000 changes of each tenant. A subscriber catches up from that log after a dropped connection, and also when it reads slower than agents write. Either way it still gets every change, in order.

A subscriber that falls more than 10,000 changes behind cannot catch up. The SDK then calls `onGap` and carries on with the writes made after that call. Read your data again inside `onGap` and you are back in step. The same happens when the subscriber is ahead of the log, which means someone replaced the database with an older copy.

`db.changes(since)` reads the same log by hand. It returns up to 500 changes after `since`, and `latest_seq`, the `seq` of the newest write. `db.changes()` returns only `latest_seq`. A `since` older than the log reaches fails with `changes_trimmed`.

To show changes in a browser, subscribe in your backend and forward them. The demo does this in `demo/app/api/stream/route.ts` with server-sent events.

### Give tools to an agent

```ts
import { generateText, isStepCount } from "ai";

const tools = await db.tools();

await generateText({
  model,
  tools,
  stopWhen: isStepCount(12),
  prompt: "Add Globex as a lead with revenue 5000",
});
```

`tools()` reads the tenant's schema and returns nine tools: `describe_database`, `get_document`, `find_documents`, `insert_document`, `update_document`, `write_documents`, `delete_document`, `ask_database` and `change_schema`.

The input schemas list each table's exact fields and allowed values, so the model sees that `status` must be `lead`, `active` or `churned` before it calls. When a call fails anyway, the tool returns the error message as its result. The model reads it and corrects itself.

`write_documents` is `batch` for the agent, and its description tells the model to prefer it for more than one document. The description of `find_documents` marks the indexed and unique fields and states the rule for tables over 1,000 documents, so the model knows which queries will run and how to add an index.

A table the agent creates with `change_schema` works with the other tools in the same conversation.

To limit what an agent may do:

```ts
await db.tools({ schemaChanges: false, deletes: false, ask: false });
```

`deletes: false` also keeps deletes out of `write_documents`.

### Handle errors

Every failure throws an `AgentDBError`.

```ts
import { AgentDBError } from "agentdb";

try {
  await db.insert("clients", { name: "Acme", emial: "a@acme.io" });
} catch (error) {
  if (error instanceof AgentDBError) {
    error.code;     // "unknown_field"
    error.message;  // the full explanation, safe to hand to an agent
    error.status;   // 400
  }
}
```

Branch on `code`, show `message`. Common codes are `unknown_table`, `unknown_field`, `wrong_type`, `missing_required`, `broken_reference`, `duplicate_value`, `not_found`, `version_conflict`, `still_referenced`, `would_destroy`, `query_needs_index` and `unreachable`.

### Types

The fixed parts are typed: operations, queries, schema changes, errors. Table and field names are strings, because each tenant has its own schema and agents change it at runtime. The database checks them when the call runs.

For tables your app defines itself, pass a type:

```ts
interface Client {
  name: string;
  status?: "lead" | "active" | "churned";
  revenue?: number;
}

const acme = await db.insert<Client>("clients", { name: "Acme", status: "lead" });
acme.status;   // "lead" | "active" | "churned" | undefined
acme.version;  // number
```

## How English queries work

`ask` uses [Jev](https://docs.typesafe.ai), a small model from TypeSafe. Jev does not write text. It picks one option from a list, or gives the probability of a yes. So it cannot write a query, and agentdb does not ask it to.

Instead, the code lists every choice the schema allows and Jev picks, all in one request:

1. Which table is this about?
2. For each fixed-choice field, which value does the request want, if any?
3. For each number in the sentence, what is it? A revenue to compare against, a result limit, a count of days?
4. Which time period, and which date field does it apply to?
5. Is this a write, a sum, an either-or, or a comparison between records? If so, refuse.

Code assembles the picks into a query. Jev reads dates as text and cannot do arithmetic, so the calendar maths for "last month" or "more than a year ago" happens in code.

Then a second, smaller request describes the built query in words and asks Jev whether the request contains anything the query leaves out. This catches a silently dropped condition. In testing, "clients in Poland" used to return every client, because clients have no country. The second check stops that.

### How well it does

`./eval.sh` scores `ask` against tasks at four levels. A task passes when `ask` returns the same documents as the expected query, or refuses when it should.

| Level | Tuned set | Unseen set |
|---|---|---|
| simple | 9 of 10 | 5 of 6 |
| normal | 12 of 13 | 7 of 9 |
| hard | 13 of 13 | 8 of 9 |
| superhard | 12 of 12 | 11 of 11 |
| Total | 46 of 48 | 31 of 35 |

I tuned the questions against the first set, so that number flatters it. I wrote the second set afterwards, and 31 of 35 is the honest score.

Neither set produced a wrong answer, meaning a query that ran and returned the wrong documents. Every miss was a refusal, and most refusals still returned the correct query as a best guess.

One request takes about 0.55 seconds and costs about $0.0003.

It handles filters on any field type, "between" ranges, sorting, top N, missing values, negation, relative and exact dates, and requests in Polish. It refuses writes, sums and averages, OR conditions, questions that span two tables and requests about nothing in the database.

The tasks are plain JSON in `evals/`. Add your own without touching Rust.

## The demo app

`demo/` is a Next.js app that uses only the SDK. It shows:

- a table view that updates live as anyone writes to the tenant
- click-to-edit cells, which surface the same errors an agent would get
- an ask box that shows how each request was understood
- an agent chat on the right, running on OpenRouter, that can create tables and edit data
- a tenant switcher, to see that tenants are separate

The agent defaults to `anthropic/claude-sonnet-5.5`. Set `AGENT_MODEL` to any OpenRouter model id to change it.

The demo's own routes hold the server secret and the OpenRouter key, so the demo lets nobody in without a password. `demo.sh` makes a random one for each run and prints it. Set `DEMO_PASSWORD` in `.env`, 12 characters or more, to keep the same one. Logging in gives the browser a cookie, and every route refuses a request that comes without it.

The app listens on every network interface, because a proxy in front of the machine has to reach it. On an exe.dev VM that proxy serves the demo at `https://<vm>.exe.xyz:3000`. Listening on localhost alone would also keep strangers out, but it would lock the proxy out with them. The password protects the demo wherever it listens. Over plain HTTP on a shared network the password travels unencrypted, so put HTTPS in front before you use it there.

The page can read and write documents and load the sample data. Only the agent chat can change a schema.

## HTTP API

The SDK is a thin wrapper over this. All tenant routes need the bearer secret and sit under `/v1/tenants/{tenant}`.

| Method and path | Body | Returns |
|---|---|---|
| `GET /health` | | `{ ok: true }`, no secret needed |
| `GET /describe` | | `{ tables }` |
| `POST /migrate` | `{ changes }` | `{ tables }` |
| `POST /tables/{table}/docs` | the document | the stored document |
| `GET /tables/{table}/docs/{id}` | | the document |
| `PATCH /tables/{table}/docs/{id}` | `{ patch, version? }` | the updated document |
| `DELETE /tables/{table}/docs/{id}?version=N` | | `{ deleted: true }`. `version` is optional. |
| `POST /batch` | `{ writes }` | `{ docs }`, one per write |
| `POST /find` | a query | `{ docs, total, next_offset }` |
| `POST /ask` | `{ text }` | `{ query, confidence, refusal, page, usage }` |
| `GET /changes?since=N` | | `{ changes, latest_seq }`, up to 500 changes. Without `since`, only `latest_seq`. |
| `GET /subscribe?since=N` | | a server-sent event stream of changes. Without `since`, it starts with the next write. |

A write in a batch is `{ "op": "insert", "table", "doc" }`, `{ "op": "update", "table", "id", "patch", "version"? }` or `{ "op": "delete", "table", "id", "version"? }`.

Each event of `/subscribe` carries one change as JSON, with its `seq` as the event id. When a subscriber has fallen further behind than the change log reaches, the last event is named `error` and carries the `changes_trimmed` error. Then the stream ends. A `since` that the log has not reached yet is refused with `since_ahead`, because following it would hide every write until the log got there. A subscriber that reads nothing for 30 seconds while changes wait for it has its stream closed, so that it cannot keep the tenant's database open. It reconnects with the last `seq` it saw.

Only `POST /migrate` creates a tenant's file. Every other route treats a tenant without a file as an empty database and leaves the disk alone.

A request body can be 8 MiB at most. A larger one is refused with `request_too_large`.

Errors come back as `{ "error": { "code": "...", "message": "..." } }` with a matching HTTP status. That is 404 for a table or document that does not exist, 409 when stored data stands in the way, such as a version conflict or a duplicate value, 410 for `changes_trimmed` and `since_ahead`, 413 for a batch or a body that is too large, and 400 for any other call that has to change. When the server itself fails, the status is 500 and the code `storage` or `internal`. The message then says only that the fault is the server's, and the detail goes to the server's log.

## Running the server

**Files.** Each tenant is one file, `<tenant>.db`. While the tenant is open, `<tenant>.db-wal` and `<tenant>.db-shm` sit next to it. The `-wal` file holds the newest writes until the database moves them into the main file, so a copy of the `.db` file alone, taken while the server runs, can miss recent writes.

**Backups.** There is no built-in backup. Stop the server first. A clean stop moves every write into the `.db` files and removes the other two, and then a copy of the data directory holds every tenant whole. Keep the master key with it. Without the key the files cannot be read.

**Stopping.** On SIGTERM or SIGINT the server stops accepting connections, ends the change feeds, gives the requests in flight five seconds to finish and closes every database. SDK subscribers reconnect on their own once it is back, and resume where they left off.

**Open tenants.** The server keeps at most `AGENTDB_MAX_OPEN_TENANTS` databases open, 100 unless you change it. To open one more it closes the one that went unused the longest, and opening that one again later takes a few milliseconds. A tenant with a request in flight or a subscriber stays open. If every open tenant is in use, a request for another gets a 503 with `too_many_open_tenants` and should retry. Each open tenant holds five file descriptors, so raise the process's descriptor limit along with the setting.

## Performance

Measured on an encrypted file with 10,000 documents and two indexed fields. Run it yourself with `cargo run --release --example bench`.

| Operation | Time |
|---|---|
| Read one document by id | 0.012 ms |
| Insert or update one document | 1.0 ms |
| Insert in batches of 500 | 0.025 ms per document |
| Filter on an indexed field, or the top 10 by one | 0.03 to 0.18 ms |
| An indexed filter that leaves 3,400 documents for `contains` to check | 3.0 ms |
| Rename a field across all 10,000 documents | 55 ms |

A single write is saved to disk on its own, which caps single writes at about 1,000 per second per tenant. A batch is saved once, so a bulk import belongs in `batch`. The 10,000 documents above took a quarter of a second.

## Limits

This is young software. Know these before you depend on it.

- **No reads across tables.** A client's `company` comes back as an id. There are no joins and no filters through a link.
- **Large tables need indexes.** A table over 1,000 documents is only searched through an index. A table has at most 10 indexed fields, and `ne` and `contains` cannot use one.
- **Deep pages are slow.** Skipping to a far `offset` costs time in proportion to the offset, and a read that runs past two seconds is stopped.
- **A batch takes 500 writes, a request 8 MiB.** Split a bigger import into several batches.
- **No OR in queries.**
- **Types are checked at runtime.** A typo in a field name fails when the call runs, not in your editor.
- **"Today" means today in UTC.** There is no per-tenant timezone yet.
- **One server process.** There is no replication and no built-in backup. Stop the server, then copy the files in the data directory.
- **100 tenants open at once.** Raise `AGENTDB_MAX_OPEN_TENANTS` and the process's file descriptor limit for more.
- **No record of who made a change.** The change feed says what changed, not which agent did it.
- **The change log keeps 10,000 changes per tenant.** A subscriber further behind than that has to read its data again.

## Develop

| Command | What it runs |
|---|---|
| `./check.sh` | Formatting, lints as errors, Rust tests, dependency audit. It must pass before a change counts as done. |
| `cd sdk && npm test` | SDK tests against the real server. Build it first with `cargo build --bin agentdb-server`. |
| `cd demo && npm test && npm run typecheck && npx next build` | Tests the demo's password check, then type-checks and builds the demo. Build the SDK first with `cd sdk && npm run build`. |
| `./eval.sh` | English-query scores. Pass `evals/holdout.json` for the unseen set. |

| Path | What is there |
|---|---|
| `src/db.rs`, `src/write.rs` | Storage, documents, batches, the change log |
| `src/schema.rs`, `src/migrate.rs` | Field types, validation, schema changes |
| `src/query.rs`, `src/index.rs` | Structured queries, their SQL and the indexes behind them |
| `src/ask.rs`, `src/ask/`, `src/jev.rs` | English queries |
| `src/server.rs`, `src/server/` | The HTTP server: routes, open tenants, the change feed |
| `src/error.rs` | Every error message, in one file |
| `sdk/` | The TypeScript SDK |
| `demo/` | The Next.js demo |
| `evals/` | English-query tasks and their seed data |

The Rust lint rules are strict on purpose, because agents write most of this code. `unsafe` is forbidden, `unwrap` is rejected outside tests, and `#[allow]` is banned so a lint cannot be silenced without a written reason. `AGENTS.md` explains the rules to coding agents.

## Licence

Apache-2.0. See `LICENSE`.
