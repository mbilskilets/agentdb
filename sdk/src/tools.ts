import type { ToolSet } from "ai";

import type { Tenant } from "./client.js";
import { AgentDBError } from "./errors.js";
import type { Field, Fields, Filter, Query, SchemaChange, TableInfo, Write } from "./types.js";

export interface ToolOptions {
  /** Let the agent create and change tables. Default true. */
  schemaChanges?: boolean;
  /** Let the agent delete documents, one at a time or in a batch. Default true. */
  deletes?: boolean;
  /** Include the English-query tool. Default true. */
  ask?: boolean;
}

type JsonSchema = Record<string, unknown>;

const SYSTEM_FIELDS = ["id", "version", "created_at", "updated_at"];
const OPS = ["eq", "ne", "gt", "gte", "lt", "lte", "contains"];

/** The JSON Schema for one field's value. */
function valueSchema(field: Field): JsonSchema {
  const described = (schema: JsonSchema, fallback?: string): JsonSchema => {
    const description = field.description ?? fallback;
    return description ? { ...schema, description } : schema;
  };
  switch (field.type) {
    case "text":
      return described({ type: "string" });
    case "number":
      return described({ type: "number" });
    case "bool":
      return described({ type: "boolean" });
    case "datetime":
      return described({ type: "string" }, "RFC 3339 timestamp like 2026-10-03T14:30:00Z, or a date like 2026-10-03");
    case "enum":
      return described({ type: "string", enum: field.values });
    case "ref":
      return described({ type: "integer" }, `id of a document in \`${field.table}\``);
  }
}

/** The shape of a new document for `table`. */
function documentSchema(table: TableInfo): JsonSchema {
  return {
    title: table.name,
    type: "object",
    properties: Object.fromEntries(table.fields.map((field) => [field.name, valueSchema(field)])),
    required: table.fields.filter((field) => field.required).map((field) => field.name),
    additionalProperties: false,
  };
}

/** The shape of an update for `table`: every field optional, `null` unsets one. */
function patchSchema(table: TableInfo): JsonSchema {
  const properties = table.fields.map((field) => {
    const schema = valueSchema(field);
    return [field.name, field.required ? schema : { anyOf: [schema, { type: "null" }] }];
  });
  return {
    title: table.name,
    type: "object",
    properties: Object.fromEntries(properties),
    additionalProperties: false,
  };
}

/**
 * A choice between the per-table shapes. The last option accepts any object,
 * for a table created earlier in the same conversation; the server still
 * checks it against that table's schema.
 */
function anyTable(tables: TableInfo[], shape: (table: TableInfo) => JsonSchema): JsonSchema {
  const fallback = {
    title: "a table created during this conversation",
    type: "object",
  };
  return { anyOf: [...tables.map(shape), fallback] };
}

function summary(tables: TableInfo[]): string {
  if (tables.length === 0) return "The database has no tables yet.";
  const lines = tables.map((table) => {
    const fields = table.fields.map((field) => {
      const kind = field.type === "enum" ? field.values.join("|") : field.type === "ref" ? `id of ${field.table}` : field.type;
      const mark = field.unique ? " (unique)" : field.indexed ? " (indexed)" : "";
      return `${field.name}${field.required ? "" : "?"}: ${kind}${mark}`;
    });
    const note = table.description ? ` (${table.description})` : "";
    return `- ${table.name}${note}: ${fields.join(", ")}`;
  });
  return `Tables (a trailing ? marks an optional field; no two documents may share the value of a unique field):\n${lines.join("\n")}\nEvery document also has id, version, created_at and updated_at, set by the database.`;
}

const FIELD_TYPE: JsonSchema = {
  type: "object",
  properties: {
    type: { type: "string", enum: ["text", "number", "bool", "datetime", "enum", "ref"] },
    values: { type: "array", items: { type: "string" }, description: "allowed values, for type enum" },
    table: { type: "string", description: "the table linked to, for type ref" },
  },
  required: ["type"],
};

const FIELD: JsonSchema = {
  type: "object",
  properties: {
    name: { type: "string", description: "lowercase letters, digits and underscores" },
    ...(FIELD_TYPE.properties as object),
    required: { type: "boolean" },
    indexed: { type: "boolean", description: "set true for a field that a table of more than 1,000 documents is filtered or sorted by" },
    unique: { type: "boolean", description: "set true when no two documents may hold the same value" },
    description: { type: "string" },
  },
  required: ["name", "type", "required"],
};

const SCHEMA_CHANGE: JsonSchema = {
  type: "object",
  description:
    "One schema change. `op` decides which other properties are needed: " +
    "define_table {table: {name, description?, fields}}; add_field {table, field}; rename_table {table, new_name}; " +
    "rename_field {table, field, new_name}; change_type {table, field, to}; set_required {table, field, required}; " +
    "set_indexed {table, field, indexed}; set_unique {table, field, unique}; " +
    "add_enum_value {table, field, value}; remove_enum_value {table, field, value}; describe {table, field?, description}; " +
    "remove_field {table, field, force?}; drop_table {table, force?}.",
  properties: {
    op: {
      type: "string",
      enum: ["define_table", "add_field", "rename_table", "rename_field", "change_type", "set_required", "set_indexed", "set_unique", "add_enum_value", "remove_enum_value", "describe", "remove_field", "drop_table"],
    },
    table: {
      description: "the table name; for define_table, the whole table definition",
      anyOf: [
        { type: "string" },
        {
          type: "object",
          properties: { name: { type: "string" }, description: { type: "string" }, fields: { type: "array", items: FIELD } },
          required: ["name", "fields"],
        },
      ],
    },
    field: { description: "a field name; for add_field, the whole field definition", anyOf: [{ type: "string" }, FIELD] },
    new_name: { type: "string" },
    to: FIELD_TYPE,
    required: { type: "boolean" },
    indexed: { type: "boolean", description: "a table has at most 10 indexed fields; unique fields and refs are always indexed" },
    unique: { type: "boolean", description: "refused while two documents hold the same value" },
    value: { type: "string" },
    description: { type: "string" },
    force: { type: "boolean", description: "only after a refusal told you how much data would be deleted, and the user wants that" },
  },
  required: ["op", "table"],
};

/** What `find_documents` tells the model about tables too large to search without an index. */
function indexRule(canChangeSchema: boolean): string {
  const rule =
    "A table with more than 1,000 documents is searched only through an index: at least one filter must compare an indexed field with eq, gt, gte, lt or lte, " +
    "and a search without filters may sort only by an indexed field. Indexed are the fields marked (indexed) or (unique) below, and always id, created_at and updated_at. " +
    "A search that breaks this rule fails with query_needs_index, and one that still has to read too much is stopped with query_too_slow.";
  const fix = ' To search by another field, index it first with change_schema: {"op": "set_indexed", "table": "<table>", "field": "<field>", "indexed": true}.';
  return canChangeSchema ? rule + fix : rule;
}

/** The shapes `write_documents` accepts for one write. Deletes are left out when the agent may not delete. */
function writeSchema(tables: TableInfo[], tableName: JsonSchema, canDelete: boolean): JsonSchema {
  const write = (op: string, properties: JsonSchema, required: string[]): JsonSchema => ({
    type: "object",
    properties: { op: { type: "string", enum: [op] }, table: tableName, ...properties },
    required: ["op", "table", ...required],
    additionalProperties: false,
  });
  const id = { type: "integer" };
  const version = { type: "integer", description: "the version you last read; the write is refused if the document changed since" };
  const shapes = [
    write("insert", { doc: anyTable(tables, documentSchema) }, ["doc"]),
    write("update", { id, patch: anyTable(tables, patchSchema), version }, ["id", "patch"]),
  ];
  if (canDelete) shapes.push(write("delete", { id, version }, ["id"]));
  return { anyOf: shapes };
}

/** Runs a tool body and hands an agentdb error to the model as the result, so it can correct the call. */
async function attempt<R>(work: () => Promise<R>): Promise<R | { error: string; code: string }> {
  try {
    return await work();
  } catch (error) {
    if (error instanceof AgentDBError) return { error: error.message, code: error.code };
    throw error;
  }
}

export async function buildTools(tenant: Tenant, options: ToolOptions = {}): Promise<ToolSet> {
  const { tool, jsonSchema } = await import("ai").catch(() => {
    throw new AgentDBError("missing_ai_package", "tools() needs the `ai` package. Install it with: npm install ai", 0);
  });
  const tables = await tenant.describe();
  const names = tables.map((table) => table.name).join(", ") || "none yet";
  const tableName = { type: "string", description: `one of: ${names}` };
  const filterFields = [...new Set(tables.flatMap((table) => table.fields.map((field) => field.name))), ...SYSTEM_FIELDS];
  const object = (properties: JsonSchema, required: string[]): JsonSchema => ({ type: "object", properties, required, additionalProperties: false });
  const canChangeSchema = options.schemaChanges ?? true;
  const canDelete = options.deletes ?? true;

  const tools: ToolSet = {
    describe_database: tool({
      description: "List every table with its fields, their types and how many documents it holds. Call this when unsure what exists.",
      inputSchema: jsonSchema<Record<string, never>>(object({}, [])),
      execute: () => attempt(() => tenant.describe()),
    }),
    get_document: tool({
      description: "Read one document by its id.",
      inputSchema: jsonSchema<{ table: string; id: number }>(object({ table: tableName, id: { type: "integer" } }, ["table", "id"])),
      execute: ({ table, id }) => attempt(() => tenant.get(table, id)),
    }),
    find_documents: tool({
      description: `Search one table with exact filters. All filters must match. Returns the documents, the total number of matches, and next_offset when there are more.\n${indexRule(canChangeSchema)}\n${summary(tables)}`,
      inputSchema: jsonSchema<Query>(
        object(
          {
            table: tableName,
            where: {
              type: "array",
              items: object(
                {
                  field: { type: "string", description: `a field of the table, such as: ${filterFields.join(", ")}` },
                  op: { type: "string", enum: OPS, description: "contains is a case-insensitive text search; gt, gte, lt, lte work on numbers and dates" },
                  value: { description: "the value to compare with. Use null with eq to find documents where the field is unset.", type: ["string", "number", "boolean", "null"] },
                },
                ["field", "op", "value"],
              ),
            },
            sort: object({ field: { type: "string" }, descending: { type: "boolean" } }, ["field"]),
            limit: { type: "integer", description: "default 50, at most 500" },
            offset: { type: "integer" },
          },
          ["table"],
        ),
      ),
      execute: (query) => attempt(() => tenant.find({ ...query, where: (query.where ?? []) as Filter[] })),
    }),
    insert_document: tool({
      description: `Add a new document to a table. Returns it with its assigned id.\n${summary(tables)}`,
      inputSchema: jsonSchema<{ table: string; document: Fields }>(object({ table: tableName, document: anyTable(tables, documentSchema) }, ["table", "document"])),
      execute: ({ table, document }) => attempt(() => tenant.insert(table, document)),
    }),
    update_document: tool({
      description: "Change some fields of one document and leave the rest alone. Set an optional field to null to remove it. Pass the version you last read to refuse the write if someone else changed the document in between.",
      inputSchema: jsonSchema<{ table: string; id: number; patch: Fields; version?: number }>(
        object({ table: tableName, id: { type: "integer" }, patch: anyTable(tables, patchSchema), version: { type: "integer" } }, ["table", "id", "patch"]),
      ),
      execute: ({ table, id, patch, version }) => attempt(() => tenant.update(table, id, patch, { version })),
    }),
    write_documents: tool({
      description:
        `Apply up to 500 writes as one unit: ${canDelete ? "inserts, updates and deletes" : "inserts and updates"}, in order, across any tables. Either every write takes effect or none does, and a failure names the write that caused it. ` +
        "Prefer this over one call per document whenever you write more than one: it is far faster, and it cannot leave the work half done. A later write sees the earlier ones. Returns one document per write, in order.",
      inputSchema: jsonSchema<{ writes: Write[] }>(object({ writes: { type: "array", items: writeSchema(tables, tableName, canDelete), minItems: 1, maxItems: 500 } }, ["writes"])),
      execute: ({ writes }) =>
        attempt(async () => {
          if (!canDelete && writes.some((write) => write.op === "delete")) {
            throw new AgentDBError("deletes_disabled", "this batch contains a delete, and you may not delete documents. Nothing was written. Send it again without the delete.", 0);
          }
          return tenant.batch(writes);
        }),
    }),
  };

  if (options.ask ?? true) {
    tools.ask_database = tool({
      description: "Search with a request in plain English, such as \"active clients created this week\". Good for simple reads. It reports the exact query it ran, and refuses with a reason when unsure; use find_documents for precise or complex filters.",
      inputSchema: jsonSchema<{ request: string }>(object({ request: { type: "string" } }, ["request"])),
      execute: ({ request }) => attempt(() => tenant.ask(request)),
    });
  }
  if (canDelete) {
    tools.delete_document = tool({
      description: "Permanently delete one document by its id. Refused while other documents link to it. Pass the version you last read to refuse the delete if someone else changed the document in between.",
      inputSchema: jsonSchema<{ table: string; id: number; version?: number }>(object({ table: tableName, id: { type: "integer" }, version: { type: "integer" } }, ["table", "id"])),
      execute: ({ table, id, version }) => attempt(async () => (await tenant.delete(table, id, { version }), { deleted: true })),
    });
  }
  if (canChangeSchema) {
    tools.change_schema = tool({
      description: "Create tables or change their shape. The changes apply in order as one unit: if any fails, none take effect. Returns the new schema. A table you create here can be used right away with the other tools.",
      inputSchema: jsonSchema<{ changes: SchemaChange[] }>(object({ changes: { type: "array", items: SCHEMA_CHANGE, minItems: 1 } }, ["changes"])),
      execute: ({ changes }) => attempt(() => tenant.migrate(changes)),
    });
  }
  return tools;
}
