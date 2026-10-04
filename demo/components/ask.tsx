"use client";

import type { Asked, Doc, TableInfo } from "agentdb";
import { type FormEvent, useState } from "react";

import { type Attempt, call } from "./api";
import { describeQuery, show } from "./format";

export function AskBar({ tenant, attempt, onAnswer }: { tenant: string; attempt: Attempt; onAnswer: (asked: Asked) => void }) {
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

export function Answer({ asked, tables, onClose }: { asked: Asked; tables: TableInfo[]; onClose: () => void }) {
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
function Rows({ table, rows }: { table: TableInfo; rows: Doc[] }) {
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
