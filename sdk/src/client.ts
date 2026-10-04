import { AgentDBError, errorFrom } from "./errors.js";
import { buildTools, type ToolOptions } from "./tools.js";
import type {
  Asked,
  Change,
  Changes,
  Doc,
  Fields,
  Page,
  Patch,
  Query,
  SchemaChange,
  TableInfo,
  Write,
} from "./types.js";

export interface AgentDBOptions {
  /** Where the agentdb server runs, such as `http://127.0.0.1:4000`. */
  url: string | undefined;
  /** The server's `AGENTDB_SECRET`. Keep it on your backend. */
  secret: string | undefined;
  /** A replacement for the global `fetch`, for tests or custom transports. */
  fetch?: typeof fetch;
}

export interface SubscribeOptions {
  /** Replay every change after this `seq` before going live. Omit to get only new changes. */
  since?: number;
  /** Called when the connection drops. The subscription reconnects by itself and resumes where it left off. */
  onError?: (error: unknown) => void;
  /**
   * Called when the change log cannot continue from where the subscription
   * stands: it fell further behind than the 10,000 changes the server
   * keeps, or the database was replaced by an older copy. Read the current
   * state again. `onChange` carries on with the writes made after this call.
   */
  onGap?: () => void;
}

const FIRST_RETRY_MS = 500;
const MAX_RETRY_MS = 5000;
/** Errors that say the change log cannot continue from the subscriber's `seq`. */
const LOST_PLACE = ["changes_trimmed", "since_ahead"];

/** A connection to an agentdb server. Call `tenant()` to work with one tenant's database. */
export class AgentDB {
  readonly url: string;
  readonly secret: string;
  readonly fetch: typeof fetch;

  constructor(options: AgentDBOptions) {
    if (!options.url) throw new AgentDBError("missing_url", "AgentDB needs the server `url`, such as http://127.0.0.1:4000.", 0);
    if (!options.secret) throw new AgentDBError("missing_secret", "AgentDB needs the server `secret` (the server's AGENTDB_SECRET).", 0);
    this.url = options.url.replace(/\/+$/, "");
    this.secret = options.secret;
    this.fetch = options.fetch ?? globalThis.fetch.bind(globalThis);
  }

  /** One tenant's database. The server creates its encrypted file with the tenant's first schema change. */
  tenant(id: string): Tenant {
    return new Tenant(this, id);
  }
}

export class Tenant {
  readonly id: string;
  readonly #db: AgentDB;
  readonly #base: string;

  constructor(db: AgentDB, id: string) {
    this.#db = db;
    this.id = id;
    this.#base = `${db.url}/v1/tenants/${encodeURIComponent(id)}`;
  }

  /** Every table with its fields and document count. */
  async describe(): Promise<TableInfo[]> {
    const body = await this.#request<{ tables: TableInfo[] }>("GET", "/describe");
    return body.tables;
  }

  /** Applies schema changes in order as one unit: if any fails, none take effect. Returns the new schema. */
  async migrate(changes: SchemaChange[]): Promise<TableInfo[]> {
    const body = await this.#request<{ tables: TableInfo[] }>("POST", "/migrate", { changes });
    return body.tables;
  }

  async insert<T = Fields>(table: string, doc: T): Promise<Doc<T>> {
    return this.#request("POST", this.#docs(table), doc);
  }

  async get<T = Fields>(table: string, id: number): Promise<Doc<T>> {
    return this.#request("GET", this.#docs(table, id));
  }

  /**
   * Changes the fields in `patch` and leaves the rest alone. Pass the
   * `version` you last read to refuse the write if someone else changed the
   * document in between.
   */
  async update<T = Fields>(table: string, id: number, patch: Patch<T>, options: { version?: number } = {}): Promise<Doc<T>> {
    return this.#request("PATCH", this.#docs(table, id), { patch, version: options.version });
  }

  /**
   * Pass the `version` you last read to refuse the delete if someone else
   * changed the document in between.
   */
  async delete(table: string, id: number, options: { version?: number } = {}): Promise<void> {
    const path = this.#docs(table, id);
    await this.#request("DELETE", options.version === undefined ? path : `${path}?version=${options.version}`);
  }

  /**
   * Applies up to 500 writes in order as one unit: if any fails, none takes
   * effect and the error names the write. Returns one document per write:
   * the document after the write, or its last state for a delete. Far
   * faster than one call per document.
   */
  async batch<T = Fields>(writes: Write[]): Promise<Doc<T>[]> {
    const body = await this.#request<{ docs: Doc<T>[] }>("POST", "/batch", { writes });
    return body.docs;
  }

  async find<T = Fields>(query: Query): Promise<Page<T>> {
    return this.#request("POST", "/find", query);
  }

  /**
   * Answers a request written in English, such as "clients created today".
   * Check `page`: it is `null` when the request was not run, and `refusal`
   * then says why. `query` shows how the request was understood either way.
   */
  async ask<T = Fields>(text: string): Promise<Asked<T>> {
    return this.#request("POST", "/ask", { text });
  }

  /**
   * Up to 500 changes with a `seq` greater than `since`, oldest first, and
   * the `seq` of the newest write. Without `since` it returns no changes,
   * only where the log stands. The server keeps the newest 10,000 changes:
   * asking for older ones fails with `changes_trimmed`.
   */
  async changes(since?: number): Promise<Changes> {
    return this.#request("GET", since === undefined ? "/changes" : `/changes?since=${since}`);
  }

  /**
   * Calls `onChange` for every write, in order, until the returned function
   * is called. Reconnects by itself and resumes where it left off. Pass
   * `onGap` if you keep a copy of the data: it says when to read it again.
   */
  subscribe(onChange: (change: Change) => void, options: SubscribeOptions = {}): () => void {
    const abort = new AbortController();
    void this.#stream(onChange, options, abort.signal);
    return () => abort.abort();
  }

  /**
   * Tools for an AI SDK agent, built from this tenant's current schema: the
   * input schemas list each table's exact fields and allowed values. Errors
   * come back to the model as the tool result, so it can fix its own call.
   * Needs the `ai` package.
   */
  async tools(options: ToolOptions = {}) {
    return buildTools(this, options);
  }

  #docs(table: string, id?: number): string {
    const path = `/tables/${encodeURIComponent(table)}/docs`;
    return id === undefined ? path : `${path}/${id}`;
  }

  async #open(method: string, path: string, body?: unknown, signal?: AbortSignal): Promise<Response> {
    let response: Response;
    try {
      response = await this.#db.fetch(this.#base + path, {
        method,
        signal,
        headers: {
          authorization: `Bearer ${this.#db.secret}`,
          ...(body === undefined ? {} : { "content-type": "application/json" }),
        },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (cause) {
      if (signal?.aborted) throw cause;
      const reason = cause instanceof Error ? cause.message : String(cause);
      throw new AgentDBError("unreachable", `could not reach agentdb at ${this.#db.url}: ${reason}. Is the server running?`, 0);
    }
    if (response.ok) return response;
    throw errorFrom(await response.text(), response.status);
  }

  async #request<R>(method: string, path: string, body?: unknown): Promise<R> {
    const response = await this.#open(method, path, body);
    return (await response.json()) as R;
  }

  async #stream(onChange: (change: Change) => void, options: SubscribeOptions, signal: AbortSignal): Promise<void> {
    let last = options.since;
    let missedChanges = false;
    let retry = FIRST_RETRY_MS;
    while (!signal.aborted) {
      try {
        if (last === undefined) {
          last = (await this.changes()).latest_seq;
          if (signal.aborted) return;
          if (missedChanges) options.onGap?.();
          missedChanges = false;
        }
        const response = await this.#open("GET", `/subscribe?since=${last}`, undefined, signal);
        retry = FIRST_RETRY_MS;
        for await (const event of serverSentEvents(response)) {
          if (event.name === "error") throw errorFrom(event.data, 0);
          const change: Change = JSON.parse(event.data);
          last = change.seq;
          onChange(change);
        }
      } catch (error) {
        if (signal.aborted) return;
        if (error instanceof AgentDBError && LOST_PLACE.includes(error.code)) {
          last = undefined;
          missedChanges = true;
          continue;
        }
        options.onError?.(error);
      }
      if (signal.aborted) return;
      await new Promise((resolve) => setTimeout(resolve, retry));
      retry = Math.min(retry * 2, MAX_RETRY_MS);
    }
  }
}

/** Yields the name and data of each server-sent event in the response body. An event without a name is a `message`. */
async function* serverSentEvents(response: Response): AsyncGenerator<{ name: string; data: string }> {
  if (!response.body) return;
  const decoder = new TextDecoder();
  let buffer = "";
  for await (const chunk of response.body) {
    buffer += decoder.decode(chunk, { stream: true });
    let end = buffer.indexOf("\n\n");
    while (end !== -1) {
      const lines = buffer.slice(0, end).split("\n");
      buffer = buffer.slice(end + 2);
      const field = (name: string) =>
        lines
          .filter((line) => line.startsWith(`${name}:`))
          .map((line) => line.slice(name.length + 1).trimStart())
          .join("\n");
      const data = field("data");
      if (data) yield { name: field("event") || "message", data };
      end = buffer.indexOf("\n\n");
    }
  }
}
