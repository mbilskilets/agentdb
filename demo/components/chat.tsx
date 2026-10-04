"use client";

import { type FormEvent, useEffect, useRef, useState } from "react";

import { request } from "./api";

interface ToolCall {
  name: string;
  input: unknown;
  output: unknown;
}

interface ChatMessage {
  role: "user" | "assistant";
  content: string;
  calls?: ToolCall[];
}

type Status = { enabled: true; model: string } | { enabled: false; reason: string };

/** The error a tool handed back to the model, if the call failed. */
function toolError({ output }: ToolCall): string | undefined {
  return typeof output === "object" && output !== null && "error" in output ? String(output.error) : undefined;
}

/** A conversation with an agent that works on the tenant through the SDK's tools. */
export function Chat({ tenant }: { tenant: string }) {
  const [status, setStatus] = useState<Status | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const end = useRef<HTMLDivElement>(null);

  useEffect(() => {
    request<Status>("/api/chat")
      .then(setStatus)
      .catch(() => setStatus({ enabled: false, reason: "The chat endpoint could not be reached." }));
  }, []);
  useEffect(() => {
    setMessages([]);
  }, [tenant]);
  useEffect(() => {
    // In braces on purpose: newer browsers return a promise from scrollIntoView,
    // and an effect must not return one.
    end.current?.scrollIntoView({ block: "end" });
  }, [messages, busy]);

  const send = async (event: FormEvent) => {
    event.preventDefault();
    const content = text.trim();
    if (!content || busy) return;
    const history: ChatMessage[] = [...messages, { role: "user", content }];
    setMessages(history);
    setText("");
    setBusy(true);
    try {
      const reply = await request<{ text: string; calls: ToolCall[] }>(`/api/chat?tenant=${encodeURIComponent(tenant)}`, {
        messages: history.map(({ role, content: body }) => ({ role, content: body })),
      });
      setMessages([...history, { role: "assistant", content: reply.text || "Done.", calls: reply.calls }]);
    } catch (failure) {
      setMessages([...history, { role: "assistant", content: `Error: ${failure instanceof Error ? failure.message : String(failure)}` }]);
    }
    setBusy(false);
  };

  return (
    <aside>
      <h2>Agent {status?.enabled && <small>{status.model}</small>}</h2>
      <div className="messages">
        {status && !status.enabled && <p className="muted">{status.reason}</p>}
        {status?.enabled && messages.length === 0 && <p className="muted">Try: “Add Wonka as a lead with revenue 9900”, or “Create a products table with a title and a price, and add three products.”</p>}
        {messages.map((message, index) => (
          <div key={index} className={`message ${message.role}`}>
            {message.calls?.map((toolCall, callIndex) => {
              const error = toolError(toolCall);
              return (
                <div key={callIndex} className={error ? "call failed" : "call"}>
                  <code>{toolCall.name}</code> {JSON.stringify(toolCall.input)}
                  {error && <div className="call-error">{error}</div>}
                </div>
              );
            })}
            <p>{message.content}</p>
          </div>
        ))}
        {busy && <p className="muted">Working…</p>}
        <div ref={end} />
      </div>
      <form onSubmit={send} className="composer">
        <input value={text} onChange={(event) => setText(event.target.value)} placeholder={status?.enabled ? "Tell the agent what to do" : "Agent is off"} disabled={!status?.enabled || busy} aria-label="Message the agent" />
        <button type="submit" disabled={!status?.enabled || busy}>
          Send
        </button>
      </form>
    </aside>
  );
}
