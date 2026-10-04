/**
 * An error from agentdb. `message` says what was wrong and how to fix the
 * call, and is written to be handed straight to an agent. `code` is a short
 * stable name for code that needs to branch, such as `unknown_field`,
 * `not_found` or `version_conflict`.
 */
export class AgentDBError extends Error {
  readonly code: string;
  /** The HTTP status, or 0 when the error did not arrive as an HTTP reply. */
  readonly status: number;

  constructor(code: string, message: string, status: number) {
    super(message);
    this.name = "AgentDBError";
    this.code = code;
    this.status = status;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** The error a server reply carries as `{"error": {"code": ..., "message": ...}}`. */
export function errorFrom(reply: string, status: number): AgentDBError {
  let body: unknown;
  try {
    body = JSON.parse(reply);
  } catch {
    body = undefined;
  }
  const error = isRecord(body) ? body.error : undefined;
  if (isRecord(error) && typeof error.code === "string" && typeof error.message === "string") {
    return new AgentDBError(error.code, error.message, status);
  }
  return new AgentDBError("http_error", `agentdb answered ${status}: ${reply}`, status);
}
