# agentdb

A small document database where every caller is an AI agent.

Agents use a database differently than people do. They misspell field names. They invent columns that sound right. Two of them overwrite the same record a second apart. And when a call fails, the error text is all they have to go on, because nobody is there to open a debugger.

Most databases answer those mistakes with errors written for a human. agentdb writes them for the agent:

```
unknown field `emial` on table `clients`. Did you mean `email`?
Valid fields: name, email, status, revenue.
```

```
`clients` id 1 is at version 2, but the update expected version 1.
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
- **A live change feed.** Every write gets a sequence number. Your UI subscribes and sees rows change as agents work.
- **Queries in English.** `ask("clients created today")` returns the documents and the exact query it ran. When it is unsure, it refuses and says why.
- **Tools for your agents.** `db.tools()` returns ready-made tools for the Vercel AI SDK, built from the tenant's current schema.

It is meant for the tables a CRM would have, such as clients, employees and deals. It is not a replacement for Postgres or Convex in a large app. See [limits](#limits) before you rely on it.

## Quick start

You need Rust and Node 22.

```bash
git clone https://github.com/mbilskilets/agentdb.git && cd agentdb

cat > .env <<'EOF'
AGENTDB_SECRET=<a long random string>
AGENTDB_MASTER_KEY=<another long random string>
TYPESAFE_API_KEY=<optional, for English queries>
OPENROUTER_API_KEY=<optional, for the demo's agent chat>
EOF

./demo.sh
```

`demo.sh` builds everything, starts the server on port 4000 and the demo app on port 3000. Open http://localhost:3000 and click "Load sample data".

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

`tenant()` costs nothing. The server creates the tenant's file the first time you use it. Tenant ids may contain letters, digits, `_` and `-`.

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
        { name: "email", type: "text", required: false },
        { name: "status", type: "enum", values: ["lead", "active", "churned"], required: false },
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
| `add_enum_value`, `remove_enum_value` | Edits the allowed values. Removal refuses while documents use the value. |
| `describe` | Sets a plain-words description on a table or field |
| `remove_field`, `drop_table` | Deletes data, so both refuse and report the damage unless you pass `force: true` |

`db.describe()` returns every table with its fields and document count. Call it first when an agent needs to find its way around.

### Read and write documents

```ts
const acme = await db.insert("clients", { name: "Acme", status: "lead" });
// { id: 1, version: 1, created_at: "...", updated_at: "...", name: "Acme", status: "lead" }

await db.get("clients", 1);

// Changes only the fields you pass. null removes an optional field.
await db.update("clients", 1, { status: "active", email: null }, { version: acme.version });

await db.delete("clients", 1);
```

Pass `version` on updates. If another agent changed the document since you read it, the write fails with `version_conflict` instead of overwriting their work.

A delete fails while other documents link to the one you are deleting. The error counts them.

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

All filters must match. The operators are `eq`, `ne`, `gt`, `gte`, `lt`, `lte` and `contains`, a case-insensitive text search. Use `value: null` with `eq` to find documents where a field is unset. You can filter and sort on `id`, `created_at` and `updated_at` too.

There is no OR and no filtering across tables yet.

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

See [how English queries work](#how-english-queries-work) for what it can do and how often it gets it right.

### Subscribe to changes

```ts
const stop = db.subscribe((change) => {
  change.seq;    // grows by one per write
  change.kind;   // "insert" | "update" | "delete" | "schema"
  change.table;
  change.doc;    // the document after the write, or null for a schema change
});

stop();
```

By default you get changes made after you subscribe. Pass `{ since: seq }` to replay everything after a known point first. The connection reconnects by itself and resumes where it left off.

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

`tools()` reads the tenant's schema and returns eight tools: `describe_database`, `get_document`, `find_documents`, `insert_document`, `update_document`, `delete_document`, `ask_database` and `change_schema`.

The input schemas list each table's exact fields and allowed values, so the model sees that `status` must be `lead`, `active` or `churned` before it calls. When a call fails anyway, the tool returns the error message as its result. The model reads it and corrects itself.

A table the agent creates with `change_schema` works with the other tools in the same conversation.

To limit what an agent may do:

```ts
await db.tools({ schemaChanges: false, deletes: false, ask: false });
```

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

Branch on `code`, show `message`. Common codes are `unknown_table`, `unknown_field`, `wrong_type`, `missing_required`, `broken_reference`, `not_found`, `version_conflict`, `still_referenced`, `would_destroy` and `unreachable`.

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
| `DELETE /tables/{table}/docs/{id}` | | `{ deleted: true }` |
| `POST /find` | a query | `{ docs, total, next_offset }` |
| `POST /ask` | `{ text }` | `{ query, confidence, refusal, page, usage }` |
| `GET /changes?since=N` | | `{ changes }`, up to 500 |
| `GET /subscribe?since=N` | | a server-sent event stream of changes |

Errors come back as `{ "error": { "code": "...", "message": "..." } }` with a matching HTTP status.

## Performance

Measured on an encrypted file with 10,000 documents. Run it yourself with `cargo run --release --example bench`.

| Operation | Time |
|---|---|
| Read one document by id | 0.03 ms |
| Insert or update one document | 4.3 ms |
| Filtered or sorted query | 3 to 6 ms |
| Rename a field across all 10,000 documents | 57 ms |

Each write is saved to disk on its own, which caps writes at about 230 per second per tenant. That is plenty for agents editing records and too slow for a bulk import.

## Limits

This is young software. Know these before you depend on it.

- **No reads across tables.** A client's `company` comes back as an id. There are no joins and no filters through a link.
- **No indexes.** Every query scans its table. That costs a few milliseconds at 10,000 documents and will hurt at a few hundred thousand.
- **No bulk insert.**
- **No OR in queries.**
- **Types are checked at runtime.** A typo in a field name fails when the call runs, not in your editor.
- **"Today" means today in UTC.** There is no per-tenant timezone yet.
- **One server process.** There is no replication and no built-in backup. Copy the files in the data directory yourself.
- **No record of who made a change.** The change feed says what changed, not which agent did it.
- **The change log grows forever.** Nothing trims it yet.

## Develop

| Command | What it runs |
|---|---|
| `./check.sh` | Formatting, lints as errors, Rust tests, dependency audit. It must pass before a change counts as done. |
| `cd sdk && npm test` | SDK tests against the real server. Build it first with `cargo build --bin agentdb-server`. |
| `./eval.sh` | English-query scores. Pass `evals/holdout.json` for the unseen set. |

| Path | What is there |
|---|---|
| `src/db.rs` | Storage, documents, the change feed |
| `src/schema.rs`, `src/migrate.rs` | Field types, validation, schema changes |
| `src/query.rs` | Structured queries and their SQL |
| `src/ask.rs`, `src/jev.rs` | English queries |
| `src/server.rs` | The HTTP server |
| `src/error.rs` | Every error message, in one file |
| `sdk/` | The TypeScript SDK |
| `demo/` | The Next.js demo |
| `evals/` | English-query tasks and their seed data |

The Rust lint rules are strict on purpose, because agents write most of this code. `unsafe` is forbidden, `unwrap` is rejected outside tests, and `#[allow]` is banned so a lint cannot be silenced without a written reason. `AGENTS.md` explains the rules to coding agents.

## Licence

Apache-2.0. See `LICENSE`.
