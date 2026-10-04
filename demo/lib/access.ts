import { createHmac, timingSafeEqual } from "node:crypto";

import type { NextRequest } from "next/server";

export const SESSION_COOKIE = "agentdb_demo_session";
export const MIN_PASSWORD_LENGTH = 12;

/**
 * The one password that lets a browser use the demo. The demo's routes hold
 * the agentdb secret and the model key, so without a password long enough
 * to resist guessing nobody gets in.
 */
function password(): string | undefined {
  const set = process.env.DEMO_PASSWORD ?? "";
  return set.length >= MIN_PASSWORD_LENGTH ? set : undefined;
}

/** What a logged-in browser holds. It is derived from the password, so the cookie never carries the password itself. */
function sessionFor(given: string): string {
  return createHmac("sha256", given).update("agentdb demo session").digest("hex");
}

function isSession(session: string, expected: string): boolean {
  const [held, wanted] = [Buffer.from(session), Buffer.from(sessionFor(expected))];
  return held.length === wanted.length && timingSafeEqual(held, wanted);
}

export function hasPassword(): boolean {
  return password() !== undefined;
}

/** The session for someone who gave the right password, or `undefined` for anyone else. */
export function logIn(given: string): string | undefined {
  const expected = password();
  const session = sessionFor(given);
  return expected !== undefined && isSession(session, expected) ? session : undefined;
}

export function isLoggedIn(request: NextRequest): boolean {
  const expected = password();
  const session = request.cookies.get(SESSION_COOKIE)?.value;
  return expected !== undefined && session !== undefined && isSession(session, expected);
}

/** The reply to an API call from a browser that has not logged in, or `undefined` when it has. */
export function refusal(request: NextRequest): Response | undefined {
  if (isLoggedIn(request)) return undefined;
  return Response.json({ error: { code: "not_logged_in", message: "Log in at /login with the demo password first." } }, { status: 401 });
}
