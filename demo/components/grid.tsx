"use client";

import type { Doc, Field, TableInfo } from "agentdb";
import { type FormEvent, useRef, useState } from "react";

import { type Attempt, call } from "./api";
import { parse, show, typeLabel } from "./format";

type Editing = { id: number; field: string } | null;

/** One table's documents, with cells that can be edited, a row to add and a button to delete. */
export function Grid({ tenant, table, rows, flash, attempt }: { tenant: string; table: TableInfo; rows: Doc[]; flash: Record<number, number>; attempt: Attempt }) {
  const [editing, setEditing] = useState<Editing>(null);
  const [draft, setDraft] = useState<Record<string, string>>({});

  const save = async (row: Doc, field: Field, raw: string) => {
    setEditing(null);
    if (raw === show(row[field.name]) || (field.type === "bool" && raw === String(row[field.name]))) return;
    await attempt(() => call(tenant, "update", { table: table.name, id: row.id, patch: { [field.name]: parse(field, raw) }, version: row.version }));
  };
  const add = async (event: FormEvent) => {
    event.preventDefault();
    const doc: Record<string, unknown> = {};
    for (const field of table.fields) {
      const raw = draft[field.name] ?? "";
      if (raw !== "") doc[field.name] = parse(field, raw);
    }
    const inserted = await attempt(() => call(tenant, "insert", { table: table.name, doc }));
    if (inserted) setDraft({});
  };

  return (
    <section>
      <div className="grid-head">
        <h2>{table.name}</h2>
        {table.description && <span className="muted">{table.description}</span>}
      </div>
      <form onSubmit={add} id="add-row" />
      <div className="scroll">
        <table>
          <thead>
            <tr>
              <th>id</th>
              {table.fields.map((field) => (
                <th key={field.name} title={field.description}>
                  {field.name}
                  {field.required && <span className="required">*</span>}
                  <small>{typeLabel(field)}</small>
                </th>
              ))}
              <th>version</th>
              <th>updated</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((row) => (
              <tr key={`${row.id}-${flash[row.id] ?? 0}`} className={flash[row.id] ? "flash" : undefined}>
                <td className="id">{row.id}</td>
                {table.fields.map((field) => {
                  const isEditing = editing?.id === row.id && editing.field === field.name;
                  return (
                    <td key={field.name} onClick={() => !isEditing && setEditing({ id: row.id, field: field.name })} className="editable">
                      {isEditing ? <CellEditor field={field} value={row[field.name]} onDone={(raw) => save(row, field, raw)} onCancel={() => setEditing(null)} /> : show(row[field.name], field)}
                    </td>
                  );
                })}
                <td className="muted">{row.version}</td>
                <td className="muted">{show(row.updated_at)}</td>
                <td>
                  <button className="quiet" onClick={() => attempt(() => call(tenant, "delete", { table: table.name, id: row.id, version: row.version }))} aria-label={`Delete ${table.name} ${row.id}`}>
                    Delete
                  </button>
                </td>
              </tr>
            ))}
            <tr className="new">
              <td className="muted">new</td>
              {table.fields.map((field) => (
                <td key={field.name}>
                  <FieldInput field={field} value={draft[field.name] ?? ""} onChange={(raw) => setDraft((previous) => ({ ...previous, [field.name]: raw }))} form="add-row" />
                </td>
              ))}
              <td colSpan={3}>
                <button type="submit" form="add-row">
                  Add
                </button>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <p className="muted hint">Click a cell to edit it. Try a wrong value, such as text in a number field, to see the error an agent would get.</p>
    </section>
  );
}

interface FieldInputProps {
  field: Field;
  value: string;
  onChange: (raw: string) => void;
  form?: string;
  /** Set when the input replaces a cell that was just clicked. */
  autoFocus?: boolean;
  onBlur?: () => void;
}

/** A dropdown for fields with a fixed set of values, a text box for the rest. */
function FieldInput({ field, value, onChange, form, autoFocus, onBlur }: FieldInputProps) {
  if (field.type !== "enum" && field.type !== "bool") {
    return <input value={value} onChange={(event) => onChange(event.target.value)} placeholder={typeLabel(field)} form={form} aria-label={field.name} />;
  }
  const options = field.type === "enum" ? field.values : ["true", "false"];
  return (
    <select value={value} onChange={(event) => onChange(event.target.value)} form={form} aria-label={field.name} autoFocus={autoFocus} onBlur={onBlur}>
      <option value="">—</option>
      {options.map((option) => (
        <option key={option} value={option}>
          {field.type === "bool" ? (option === "true" ? "yes" : "no") : option}
        </option>
      ))}
    </select>
  );
}

function CellEditor({ field, value, onDone, onCancel }: { field: Field; value: unknown; onDone: (raw: string) => void; onCancel: () => void }) {
  const [raw, setRaw] = useState(value === undefined || value === null ? "" : String(value));
  // Saving removes the input, which fires blur: without this the edit would be sent twice.
  const finished = useRef(false);
  const finish = (next: string) => {
    if (finished.current) return;
    finished.current = true;
    onDone(next);
  };
  if (field.type === "enum" || field.type === "bool") {
    return <FieldInput field={field} value={raw} onChange={finish} autoFocus onBlur={() => !finished.current && onCancel()} />;
  }
  return (
    <input
      autoFocus
      value={raw}
      onChange={(event) => setRaw(event.target.value)}
      onBlur={() => finish(raw)}
      onKeyDown={(event) => {
        if (event.key === "Enter") finish(raw);
        if (event.key === "Escape") {
          finished.current = true;
          onCancel();
        }
      }}
    />
  );
}
