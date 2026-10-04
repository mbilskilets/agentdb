import type { NextRequest } from "next/server";

import { refusal } from "@/lib/access";
import { tenantFrom } from "@/lib/db";

export const dynamic = "force-dynamic";

const HEARTBEAT_MS = 15_000;

/**
 * Relays the tenant's change feed to the browser as server-sent events. The
 * browser never talks to agentdb itself: it would need the server secret.
 * A `gap` event tells the browser that it missed changes and has to load
 * what it shows again.
 */
export function GET(request: NextRequest) {
  const refused = refusal(request);
  if (refused) return refused;
  const db = tenantFrom(request);
  const encoder = new TextEncoder();
  let close = () => {};
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      const send = (text: string) => {
        try {
          controller.enqueue(encoder.encode(text));
        } catch {
          close();
        }
      };
      send(": connected\n\n");
      const stop = db.subscribe((change) => send(`data: ${JSON.stringify(change)}\n\n`), {
        onGap: () => send("event: gap\ndata: {}\n\n"),
      });
      const heartbeat = setInterval(() => send(": ping\n\n"), HEARTBEAT_MS);
      close = () => {
        stop();
        clearInterval(heartbeat);
        close = () => {};
      };
      request.signal.addEventListener("abort", () => close());
    },
    cancel() {
      close();
    },
  });
  return new Response(stream, {
    headers: {
      "content-type": "text/event-stream",
      "cache-control": "no-cache, no-transform",
      "x-accel-buffering": "no",
    },
  });
}
