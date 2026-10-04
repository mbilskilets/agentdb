import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import { NextRequest } from "next/server.js";

import { hasPassword, isLoggedIn, logIn, refusal, SESSION_COOKIE } from "../lib/access.ts";

const PASSWORD = "a long demo password";

function request(session?: string): NextRequest {
  return new NextRequest("http://demo.test/api/db", { headers: session === undefined ? {} : { cookie: `${SESSION_COOKIE}=${session}` } });
}

afterEach(() => {
  delete process.env.DEMO_PASSWORD;
});

test("the right password gives a session that the routes accept", async () => {
  process.env.DEMO_PASSWORD = PASSWORD;
  const session = logIn(PASSWORD);
  assert.ok(session);
  assert.notEqual(session, PASSWORD);
  assert.equal(isLoggedIn(request(session)), true);
  assert.equal(refusal(request(session)), undefined);
});

test("a wrong password, a missing session and a forged one are refused", async () => {
  process.env.DEMO_PASSWORD = PASSWORD;
  assert.equal(logIn("a wrong demo password"), undefined);
  for (const stranger of [request(), request(""), request(PASSWORD), request("0".repeat(64))]) {
    const refused = refusal(stranger);
    assert.equal(refused?.status, 401);
    assert.deepEqual(await refused?.json(), { error: { code: "not_logged_in", message: "Log in at /login with the demo password first." } });
  }
});

test("without a password of 12 characters nobody gets in", () => {
  for (const unusable of [undefined, "", "short"]) {
    if (unusable === undefined) delete process.env.DEMO_PASSWORD;
    else process.env.DEMO_PASSWORD = unusable;
    assert.equal(hasPassword(), false);
    assert.equal(logIn(unusable ?? ""), undefined);
    assert.equal(refusal(request())?.status, 401);
  }
});
