import { NextResponse, type NextRequest } from "next/server";

import { isLoggedIn } from "@/lib/access";

/** Sends a browser that has not logged in to the login page. The API routes refuse such a browser themselves. */
export function proxy(request: NextRequest) {
  if (!isLoggedIn(request)) return NextResponse.redirect(new URL("/login", request.url));
}

export const config = { matcher: "/" };
