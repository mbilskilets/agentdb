"use client";

import type { Asked, Change, Doc, Field, Query, TableInfo } from "agentdb";
import { type FormEvent, useCallback, useEffect, useRef, useState } from "react";

type Row = Doc;
type Editing = { id: number; field: string } | null;
interface ChatMessage {
  role: "user" | "assistant";
  content: string;
  calls?: { name: string; input: unknown; output: unknown }[];
}

const PAGE_SIZE = 200;
const LOG_SIZE = 40;

/** Calls the app's own API, which holds the agentdb secret. */
async function call<T>(tenant: string, action: string, args: object = {}): Promise<T> {
  const response = await fetch(`/api/db?tenant=${encodeURIComponent(tenant)}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ action, ...args }),
  });
  const body = await response.json();
  if (!response.ok) throw new Error(body.error?.message ?? "the request failed");
  return body as T;
}

/** Turns what was typed into the value the field's type expects. Empty means "no value". */
function parse(field: Field, raw: string): unknown {
  if (raw === "") return null;
  if (field.type === "number" || field.type === "ref") return Number.isNaN(Number(raw)) ? raw : Number(raw);
  if (field.type === "bool") return raw === "true";
  return raw;
}

function show(value: unknown, field?: Field): string {
  if (value === undefined || value === null) return "";
  if (typeof value === "boolean") return value ? "yes" : "no";
  if (field?.type === "ref") return `#${value}`;
  if (field?.type === "datetime" || (typeof value === "string" && /^\d{4}-\d\d-\d\dT/.test(value))) {
    return String(value).replace("T", " ").replace(":00Z", "").replace("Z", "");
  }
  return String(value);
}

function typeLabel(field: Field): string {
  if (field.type === "enum") return field.values.join(" | ");
  if (field.type === "ref") return `→ ${field.table}`;
  return field.type;
}

function describeQuery(query: Query): string[] {
  const words: Record<string, string> = { eq: "is", ne: "is not", gt: ">", gte: "≥", lt: "<", lte: "≤", contains: "contains" };
  const parts = [query.table];
  for (const filter of query.where ?? []) {
    parts.push(filter.value === null ? `${filter.field} ${filter.op === "eq" ? "is empty" : "is set"}` : `${filter.field} ${words[filter.op]} ${filter.value}`);
  }
  if (query.sort) parts.push(`sorted by ${query.sort.field} ${query.sort.descending ? "↓" : "↑"}`);
  if (query.limit) parts.push(`first ${query.limit}`);
  return parts;
}

export default function Home() {
  const [tenant, setTenant] = useState<string | null>(null);
  const [tenantInput, setTenantInput] = useState("demo");
  const [tables, setTables] = useState<TableInfo[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [rows, setRows] = useState<Row[]>([]);
  const [flash, setFlash] = useState<Record<number, number>>({});
  const [log, setLog] = useState<Change[]>([]);
  const [live, setLive] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [asked, setAsked] = useState<Asked | null>(null);
  const [busy, setBusy] = useState(false);
  const activeRef = useRef<string | null>(null);
  /** Rows deleted since the last full load started, so a slow load cannot bring them back. */
  const deleted = useRef(new Set<number>());
  activeRef.current = active;

  /** Runs a database call and shows its error message, word for word, if it fails. */
  const attempt = useCallback(async <T,>(work: () => Promise<T>): Promise<T | undefined> => {
    try {
      const result = await work();
      setError(null);
      return result;
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : String(failure));
      return undefined;
    }
  }, []);

  const refreshTables = useCallback(
    async (of: string) => {
      const found = await attempt(() => call<TableInfo[]>(of, "describe"));
      if (!found) return;
      setTables(found);
      setActive((current) => (found.some((table) => table.name === current) ? current : (found[0]?.name ?? null)));
    },
    [attempt],
  );

  const loadRows = useCallback(
    async (of: string, table: string) => {
      deleted.current = new Set();
      const page = await attempt(() => call<{ docs: Row[] }>(of, "find", { query: { table, limit: PAGE_SIZE } }));
      if (!page || activeRef.current !== table) return;
      // Live changes may have arrived while this was loading: keep whichever copy of a row is newer.
      setRows((live) => {
        const merged = new Map(page.docs.filter((row) => !deleted.current.has(row.id)).map((row) => [row.id, row]));
        for (const row of live) if (row.version >= (merged.get(row.id)?.version ?? 0)) merged.set(row.id, row);
        return [...merged.values()].sort((a, b) => a.id - b.id);
      });
    },
    [attempt],
  );

  useEffect(() => {
    const initial = new URLSearchParams(window.location.search).get("tenant") || "demo";
    setTenant(initial);
    setTenantInput(initial);
  }, []);

  useEffect(() => {
    if (!tenant) return;
    setTables([]);
    setActive(null);
    setRows([]);
    setLog([]);
    setAsked(null);
    void refreshTables(tenant);
  }, [tenant, refreshTables]);

  useEffect(() => {
    setRows([]);
    if (tenant && active) void loadRows(tenant, active);
  }, [tenant, active, loadRows]);

  useEffect(() => {
    if (!tenant) return;
    const source = new EventSource(`/api/stream?tenant=${encodeURIComponent(tenant)}`);
    source.onopen = () => setLive(true);
    source.onerror = () => setLive(false);
    source.onmessage = (event) => {
      const change = JSON.parse(event.data) as Change;
      setLog((previous) => [change, ...previous].slice(0, LOG_SIZE));
      if (change.kind === "schema" || !change.doc) {
        void refreshTables(tenant);
        if (activeRef.current) void loadRows(tenant, activeRef.current);
        return;
      }
      const doc = change.doc;
      const step = change.kind === "insert" ? 1 : change.kind === "delete" ? -1 : 0;
      setTables((previous) => previous.map((table) => (table.name === change.table ? { ...table, count: table.count + step } : table)));
      if (change.table !== activeRef.current) return;
      if (change.kind === "delete") deleted.current.add(doc.id);
      setRows((previous) => {
        const others = previous.filter((row) => row.id !== doc.id);
        return change.kind === "delete" ? others : [...others, doc].sort((a, b) => a.id - b.id);
      });
      setFlash((previous) => ({ ...previous, [doc.id]: change.seq }));
    };
    return () => {
      source.close();
      setLive(false);
    };
  }, [tenant, refreshTables, loadRows]);

  if (!tenant) return null;
  const table = tables.find((candidate) => candidate.name === active);

  const openTenant = (event: FormEvent) => {
    event.preventDefault();
    const next = tenantInput.trim() || "demo";
    window.history.replaceState(null, "", `?tenant=${encodeURIComponent(next)}`);
    setTenant(next);
  };
  const loadSample = async () => {
    setBusy(true);
    await attempt(() => call(tenant, "sample"));
    setBusy(false);
  };

  return (
    <div className="shell">
      <header>
        <h1>agentdb</h1>
        <form onSubmit={openTenant} className="tenant">
          <label htmlFor="tenant">tenant</label>
          <input id="tenant" value={tenantInput} onChange={(event) => setTenantInput(event.target.value)} spellCheck={false} />
          <button type="submit">Open</button>
        </form>
        <span className={live ? "status live" : "status"}>{live ? "live" : "connecting…"}</span>
      </header>

      <nav>
        <h2>Tables</h2>
        {tables.map((entry) => (
          <button key={entry.name} className={entry.name === active ? "table active" : "table"} onClick={() => (setActive(entry.name), setAsked(null))}>
            <span>{entry.name}</span>
            <span className="count">{entry.count}</span>
          </button>
        ))}
        {tables.length === 0 && <p className="muted">This tenant has no tables yet.</p>}
        <h2>Activity</h2>
        <ol className="log">
          {log.map((change) => (
            <li key={change.seq}>
              <span className={`kind ${change.kind}`}>{change.kind}</span> {change.table}
              {change.doc ? ` #${change.doc.id}` : ""}
            </li>
          ))}
          {log.length === 0 && <li className="muted">Changes appear here as they happen.</li>}
        </ol>
      </nav>

      <main>
        {error && (
          <div className="error" role="alert">
            <span>{error}</span>
            <button onClick={() => setError(null)} aria-label="Dismiss">
              ×
            </button>
          </div>
        )}
        {tables.length === 0 ? (
          <div className="empty">
            <p>Start with three linked tables (companies, employees, clients), or ask the agent to create your own.</p>
            <button className="primary" onClick={loadSample} disabled={busy}>
              {busy ? "Loading…" : "Load sample data"}
            </button>
          </div>
        ) : (
          <>
            <AskBar tenant={tenant} attempt={attempt} onAnswer={setAsked} />
            {asked ? (
              <Answer asked={asked} tables={tables} onClose={() => setAsked(null)} />
            ) : (
              table && <Grid key={table.name} tenant={tenant} table={table} rows={rows} flash={flash} attempt={attempt} />
            )}
          </>
        )}
      </main>

      <Chat tenant={tenant} />
    </div>
  );
}

type Attempt = <T>(work: () => Promise<T>) => Promise<T | undefined>;

function AskBar({ tenant, attempt, onAnswer }: { tenant: string; attempt: Attempt; onAnswer: (asked: Asked) => void }) {
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!text.trim()) return;
    setBusy(true);
    const asked = await attempt(() => call<Asked>(tenant, "ask", { text }));
    setBusy(false);
    if (asked) onAnswer(asked);
  };
  return (
    <form className="ask" onSubmit={submit}>
      <input value={text} onChange={(event) => setText(event.target.value)} placeholder="Ask in English, e.g. active clients with revenue over 5000" aria-label="Ask in English" />
      <button type="submit" disabled={busy}>
        {busy ? "Asking…" : "Ask"}
      </button>
    </form>
  );
}

function Answer({ asked, tables, onClose }: { asked: Asked; tables: TableInfo[]; onClose: () => void }) {
  const table = tables.find((candidate) => candidate.name === asked.query?.table);
  return (
    <section className="answer">
      <div className="answer-head">
        {asked.query ? (
          <div className="chips">
            <span className="muted">Understood as</span>
            {describeQuery(asked.query).map((part, index) => (
              <span className="chip" key={index}>
                {part}
              </span>
            ))}
            <span className="muted">confidence {asked.confidence.toFixed(2)}</span>
          </div>
        ) : (
          <span className="muted">No query was built.</span>
        )}
        <button onClick={onClose}>Back to table</button>
      </div>
      {asked.refusal && <p className="refusal">Not run: {asked.refusal}</p>}
      {asked.page && table && (
        <>
          <p className="muted">
            {asked.page.total} match{asked.page.total === 1 ? "" : "es"}
          </p>
          <Rows table={table} rows={asked.page.docs} />
        </>
      )}
    </section>
  );
}

/** A read-only table of documents. */
function Rows({ table, rows }: { table: TableInfo; rows: Row[] }) {
  return (
    <div className="scroll">
      <table>
        <thead>
          <tr>
            <th>id</th>
            {table.fields.map((field) => (
              <th key={field.name}>{field.name}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={row.id}>
              <td className="id">{row.id}</td>
              {table.fields.map((field) => (
                <td key={field.name}>{show(row[field.name], field)}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function Grid({ tenant, table, rows, flash, attempt }: { tenant: string; table: TableInfo; rows: Row[]; flash: Record<number, number>; attempt: Attempt }) {
  const [editing, setEditing] = useState<Editing>(null);
  const [draft, setDraft] = useState<Record<string, string>>({});

  const save = async (row: Row, field: Field, raw: string) => {
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
                  <button className="quiet" onClick={() => attempt(() => call(tenant, "delete", { table: table.name, id: row.id }))} aria-label={`Delete ${table.name} ${row.id}`}>
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

function Chat({ tenant }: { tenant: string }) {
  const [status, setStatus] = useState<{ enabled: boolean; model?: string; reason?: string } | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const end = useRef<HTMLDivElement>(null);

  useEffect(() => {
    fetch("/api/chat")
      .then((response) => response.json())
      .then(setStatus)
      .catch(() => setStatus({ enabled: false, reason: "The chat endpoint could not be reached." }));
  }, []);
  useEffect(() => {
    setMessages([]);
  }, [tenant]);
  useEffect(() => {
    // In braces on purpose: newer browsers return a promise from scrollIntoView,
    // and an effect must not return one.
    end.current?.scrollIntoView({ block: "end" });
  }, [messages, busy]);

  const send = async (event: FormEvent) => {
    event.preventDefault();
    const content = text.trim();
    if (!content || busy) return;
    const history: ChatMessage[] = [...messages, { role: "user", content }];
    setMessages(history);
    setText("");
    setBusy(true);
    try {
      const response = await fetch(`/api/chat?tenant=${encodeURIComponent(tenant)}`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ messages: history.map(({ role, content: body }) => ({ role, content: body })) }),
      });
      const body = await response.json();
      const reply: ChatMessage = response.ok ? { role: "assistant", content: body.text || "Done.", calls: body.calls } : { role: "assistant", content: `Error: ${body.error?.message ?? "the request failed"}` };
      setMessages([...history, reply]);
    } catch (failure) {
      setMessages([...history, { role: "assistant", content: `Error: ${failure instanceof Error ? failure.message : String(failure)}` }]);
    }
    setBusy(false);
  };

  return (
    <aside>
      <h2>
        Agent {status?.enabled && <small>{status.model}</small>}
      </h2>
      <div className="messages">
        {status && !status.enabled && <p className="muted">{status.reason}</p>}
        {status?.enabled && messages.length === 0 && <p className="muted">Try: “Add Wonka as a lead with revenue 9900”, or “Create a products table with a title and a price, and add three products.”</p>}
        {messages.map((message, index) => (
          <div key={index} className={`message ${message.role}`}>
            {message.calls?.map((toolCall, callIndex) => {
              const failed = typeof toolCall.output === "object" && toolCall.output !== null && "error" in toolCall.output;
              return (
                <div key={callIndex} className={failed ? "call failed" : "call"}>
                  <code>{toolCall.name}</code> {JSON.stringify(toolCall.input)}
                  {failed && <div className="call-error">{String((toolCall.output as { error: unknown }).error)}</div>}
                </div>
              );
            })}
            <p>{message.content}</p>
          </div>
        ))}
        {busy && <p className="muted">Working…</p>}
        <div ref={end} />
      </div>
      <form onSubmit={send} className="composer">
        <input value={text} onChange={(event) => setText(event.target.value)} placeholder={status?.enabled ? "Tell the agent what to do" : "Agent is off"} disabled={!status?.enabled || busy} aria-label="Message the agent" />
        <button type="submit" disabled={!status?.enabled || busy}>
          Send
        </button>
      </form>
    </aside>
  );
}
