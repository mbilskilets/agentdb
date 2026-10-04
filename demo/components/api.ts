/** Runs a call and shows its error message, word for word, if it fails. */
export type Attempt = <T>(work: () => Promise<T>) => Promise<T | undefined>;

function errorMessage(reply: unknown): string | undefined {
  if (typeof reply !== "object" || reply === null || !("error" in reply)) return undefined;
  const { error } = reply;
  return typeof error === "object" && error !== null && "message" in error && typeof error.message === "string" ? error.message : undefined;
}

/**
 * Calls one of the demo's own routes, which hold the agentdb secret, and
 * returns its JSON. With a `body` the call is a POST. A browser that is no
 * longer logged in goes back to the login page.
 */
export async function request<T>(path: string, body?: object): Promise<T> {
  const response = await fetch(path, body && { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  if (response.status === 401) window.location.assign("/login");
  const reply: unknown = await response.json();
  if (!response.ok) throw new Error(errorMessage(reply) ?? "the request failed");
  return reply as T;
}

/** Runs one database call for a tenant. */
export function call<T>(tenant: string, action: string, args: object = {}): Promise<T> {
  return request(`/api/db?tenant=${encodeURIComponent(tenant)}`, { action, ...args });
}
