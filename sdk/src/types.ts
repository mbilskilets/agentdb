/** The kind of value a field holds. */
export type FieldType =
  | { type: "text" }
  | { type: "number" }
  | { type: "bool" }
  /** An RFC 3339 timestamp or a plain `YYYY-MM-DD` date. Stored in UTC. */
  | { type: "datetime" }
  | { type: "enum"; values: string[] }
  /** The id of a document in another table. */
  | { type: "ref"; table: string };

export type Field = FieldType & {
  name: string;
  required: boolean;
  /** What the field means, in plain words. */
  description?: string;
};

export interface TableDef {
  name: string;
  /** What a document in this table is, in plain words. */
  description?: string;
  fields: Field[];
}

/** A table as `describe()` reports it. */
export interface TableInfo {
  name: string;
  description: string | null;
  /** How many documents the table holds. */
  count: number;
  fields: Field[];
}

/** The fields the database sets on every document. */
export interface SystemFields {
  id: number;
  /** Grows by one on every update. Pass it back to guard against overwrites. */
  version: number;
  created_at: string;
  updated_at: string;
}

export type Fields = Record<string, unknown>;

/** A stored document: your fields plus the ones the database sets. */
export type Doc<T = Fields> = T & SystemFields;

/** A partial update. `null` removes an optional field. */
export type Patch<T = Fields> = { [K in keyof T]?: T[K] | null };

export type Op = "eq" | "ne" | "gt" | "gte" | "lt" | "lte" | "contains";

export interface Filter {
  field: string;
  op: Op;
  /** `null` with `eq` finds documents where the field is unset. */
  value: string | number | boolean | null;
}

/** A read of one table. All filters must match. */
export interface Query {
  table: string;
  where?: Filter[];
  sort?: { field: string; descending?: boolean } | null;
  /** Defaults to 50, capped at 500. */
  limit?: number | null;
  offset?: number;
}

export interface Page<T = Fields> {
  docs: Doc<T>[];
  /** How many documents match in total, across all pages. */
  total: number;
  /** Pass as the next query's `offset`. `null` on the last page. */
  next_offset: number | null;
}

/** The result of an English request. */
export interface Asked<T = Fields> {
  /** How the request was understood. Present even when it was not run. */
  query: Query | null;
  /** The model's weakest answer behind the query, from 0 to 1. */
  confidence: number;
  /** Why the query was not run. `null` when it was. */
  refusal: string | null;
  /** The results. `null` when the query was not run. */
  page: Page<T> | null;
  usage: { input_tokens: number; output_tokens: number };
}

export type ChangeKind = "insert" | "update" | "delete" | "schema";

/** One committed write. `seq` grows by one per write. */
export interface Change<T = Fields> {
  seq: number;
  table: string;
  kind: ChangeKind;
  at: string;
  /** The document after the write, or its last state for a delete. `null` for a schema change. */
  doc: Doc<T> | null;
}

/** One change to the schema. Several passed to `migrate` apply as a unit. */
export type SchemaChange =
  | { op: "define_table"; table: TableDef }
  | { op: "add_field"; table: string; field: Field }
  | { op: "rename_table"; table: string; new_name: string }
  | { op: "rename_field"; table: string; field: string; new_name: string }
  | { op: "change_type"; table: string; field: string; to: FieldType }
  | { op: "set_required"; table: string; field: string; required: boolean }
  | { op: "add_enum_value"; table: string; field: string; value: string }
  | { op: "remove_enum_value"; table: string; field: string; value: string }
  | { op: "describe"; table: string; field?: string; description: string }
  /** Refuses when data would be lost, unless `force` is true. */
  | { op: "remove_field"; table: string; field: string; force?: boolean }
  /** Refuses when the table holds documents, unless `force` is true. */
  | { op: "drop_table"; table: string; force?: boolean };
