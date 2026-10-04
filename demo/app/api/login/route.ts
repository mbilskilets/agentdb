import { NextResponse, type NextRequest } from "next/server";

import { logIn, SESSION_COOKIE } from "@/lib/access";

export const dynamic = "force-dynamic";

const WEEK_IN_SECONDS = 7 * 24 * 60 * 60;

function seeOther(location: string): NextResponse {
  return new NextResponse(null, { status: 303, headers: { location } });
}

/** Checks the password from the login form and, when it is right, gives the browser its session. */
export async function POST(request: NextRequest) {
  const given = (await request.formData()).get("password");
  const session = typeof given === "string" ? logIn(given) : undefined;
  if (!session) return seeOther("/login?wrong");
  const response = seeOther("/");
  response.cookies.set(SESSION_COOKIE, session, {
    httpOnly: true,
    sameSite: "lax",
    secure: request.headers.get("x-forwarded-proto") === "https",
    path: "/",
    maxAge: WEEK_IN_SECONDS,
  });
  return response;
}
