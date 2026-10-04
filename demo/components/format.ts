import type { Field, Query } from "agentdb";

/** Turns what was typed into the value the field's type expects. Empty means "no value". */
export function parse(field: Field, raw: string): unknown {
  if (raw === "") return null;
  if (field.type === "number" || field.type === "ref") return Number.isNaN(Number(raw)) ? raw : Number(raw);
  if (field.type === "bool") return raw === "true";
  return raw;
}

export function show(value: unknown, field?: Field): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "boolean") return value ? "yes" : "no";
  if (field?.type === "ref") return `#${value}`;
  if (field?.type === "datetime" || (typeof value === "string" && /^\d{4}-\d\d-\d\dT/.test(value))) {
    return String(value).replace("T", " ").replace(":00Z", "").replace("Z", "");
  }
  return String(value);
}

export function typeLabel(field: Field): string {
  const kind = field.type === "enum" ? field.values.join(" | ") : field.type === "ref" ? `→ ${field.table}` : field.type;
  const index = field.unique ? " · unique" : field.indexed ? " · indexed" : "";
  return kind + index;
}

export function describeQuery(query: Query): string[] {
  const words: Record<string, string> = { eq: "is", ne: "is not", gt: ">", gte: "≥", lt: "<", lte: "≤", contains: "contains" };
  const parts = [query.table];
  for (const filter of query.where ?? []) {
    parts.push(filter.value === null ? `${filter.field} ${filter.op === "eq" ? "is empty" : "is set"}` : `${filter.field} ${words[filter.op]} ${filter.value}`);
  }
  if (query.sort) parts.push(`sorted by ${query.sort.field} ${query.sort.descending ? "↓" : "↑"}`);
  if (query.limit) parts.push(`first ${query.limit}`);
  return parts;
}
