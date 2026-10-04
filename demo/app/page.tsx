"use client";

import type { Asked, Change, Doc, TableInfo } from "agentdb";
import { type FormEvent, useCallback, useEffect, useRef, useState } from "react";

import { type Attempt, call } from "@/components/api";
import { Answer, AskBar } from "@/components/ask";
import { Chat } from "@/components/chat";
import { Grid } from "@/components/grid";

const PAGE_SIZE = 200;
const LOG_SIZE = 40;

export default function Home() {
  const [tenant, setTenant] = useState<string | null>(null);
  const [tenantInput, setTenantInput] = useState("demo");
  const [tables, setTables] = useState<TableInfo[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [rows, setRows] = useState<Doc[]>([]);
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

  const attempt: Attempt = useCallback(async <T,>(work: () => Promise<T>): Promise<T | undefined> => {
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
      const page = await attempt(() => call<{ docs: Doc[] }>(of, "find", { table, limit: PAGE_SIZE }));
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
    const loadAgain = () => {
      void refreshTables(tenant);
      if (activeRef.current) void loadRows(tenant, activeRef.current);
    };
    const source = new EventSource(`/api/stream?tenant=${encodeURIComponent(tenant)}`);
    source.onopen = () => setLive(true);
    source.onerror = () => setLive(false);
    source.addEventListener("gap", loadAgain);
    source.onmessage = (event) => {
      const change: Change = JSON.parse(event.data);
      setLog((previous) => [change, ...previous].slice(0, LOG_SIZE));
      if (change.kind === "schema" || !change.doc) {
        loadAgain();
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
