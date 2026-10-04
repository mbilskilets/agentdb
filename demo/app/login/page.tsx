import { hasPassword, MIN_PASSWORD_LENGTH } from "@/lib/access";

export const dynamic = "force-dynamic";

export default async function Login({ searchParams }: { searchParams: Promise<{ [key: string]: string | string[] | undefined }> }) {
  const wrong = "wrong" in (await searchParams);
  return (
    <main className="login">
      <h1>agentdb</h1>
      {hasPassword() ? (
        <form method="post" action="/api/login">
          <label htmlFor="password">Demo password</label>
          <input id="password" name="password" type="password" autoFocus required />
          <button type="submit" className="primary">
            Log in
          </button>
          {wrong && (
            <p className="refusal" role="alert">
              That is not the password.
            </p>
          )}
          <p className="muted">
            <code>demo.sh</code> prints the password when it starts.
          </p>
        </form>
      ) : (
        <p className="refusal" role="alert">
          The demo has no password of at least {MIN_PASSWORD_LENGTH} characters, so it lets nobody in. Start it with <code>./demo.sh</code>, or set <code>DEMO_PASSWORD</code> yourself.
        </p>
      )}
    </main>
  );
}
