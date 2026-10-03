/**
 * An error from agentdb. `message` says what was wrong and how to fix the
 * call, and is written to be handed straight to an agent. `code` is a short
 * stable name for code that needs to branch, such as `unknown_field`,
 * `not_found` or `version_conflict`.
 */
export class AgentDBError extends Error {
  readonly code: string;
  /** The HTTP status, or 0 when the server could not be reached. */
  readonly status: number;

  constructor(code: string, message: string, status: number) {
    super(message);
    this.name = "AgentDBError";
    this.code = code;
    this.status = status;
  }
}
