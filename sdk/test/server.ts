import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

export const SECRET = "sdk test secret";
const ROOT = join(import.meta.dirname, "..", "..");
const BINARY = join(ROOT, "target", "debug", "agentdb-server");

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const probe = createServer();
    probe.once("error", reject);
    probe.listen(0, "127.0.0.1", () => {
      const address = probe.address();
      probe.close(() => (typeof address === "object" && address ? resolve(address.port) : reject(new Error("no port"))));
    });
  });
}

/** The TypeSafe key from the repository's `.env`, when there is one. */
export function typesafeKey(): string | undefined {
  if (process.env.TYPESAFE_API_KEY) return process.env.TYPESAFE_API_KEY;
  try {
    const line = readFileSync(join(ROOT, ".env"), "utf8").split("\n").find((entry) => entry.startsWith("TYPESAFE_API_KEY="));
    return line?.slice("TYPESAFE_API_KEY=".length).trim() || undefined;
  } catch {
    return undefined;
  }
}

/** Starts the real server on a free port with an empty data directory. */
export async function startServer(): Promise<{ url: string; stop: () => void }> {
  const port = await freePort();
  const key = typesafeKey();
  const child: ChildProcess = spawn(BINARY, [], {
    env: {
      PATH: process.env.PATH,
      AGENTDB_SECRET: SECRET,
      AGENTDB_MASTER_KEY: "sdk test master key",
      AGENTDB_DATA_DIR: mkdtempSync(join(tmpdir(), "agentdb-sdk-")),
      AGENTDB_PORT: String(port),
      ...(key ? { TYPESAFE_API_KEY: key } : {}),
    },
    stdio: "ignore",
  });
  const url = `http://127.0.0.1:${port}`;
  for (let attempt = 0; attempt < 100; attempt++) {
    try {
      if ((await fetch(`${url}/health`)).ok) return { url, stop: () => child.kill() };
    } catch {
      // Not listening yet.
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  child.kill();
  throw new Error(`agentdb-server did not start. Build it first: cargo build --bin agentdb-server (looked for ${BINARY})`);
}
