import { createOpenRouter } from "@openrouter/ai-sdk-provider";
import { generateText, isStepCount, type LanguageModel } from "ai";
import type { NextRequest } from "next/server";
import { z } from "zod";

import { refusal } from "@/lib/access";
import { errorResponse, tenantFrom } from "@/lib/db";

export const dynamic = "force-dynamic";

const MAX_STEPS = 12;
const MAX_MESSAGES = 40;
const MAX_MESSAGE_CHARS = 4000;
const INSTRUCTIONS = `You manage a small business database for the user through your tools.
Work out what exists with describe_database when unsure. Create or reshape tables with change_schema when asked.
When a tool returns an error, read it: it says how to fix the call. Fix the call and try again.
Write several documents at once with write_documents instead of one call each.
Never pass force: true unless the user has confirmed, after seeing how much data would be deleted.
When you are done, answer in one or two short sentences. Do not repeat the data back: the user sees the table update live.`;

const DEFAULT_MODEL = "anthropic/claude-sonnet-5.5";

/** A conversation as the page sends it. The limits cap what one request can spend on the model. */
const Conversation = z.object({
  messages: z
    .array(z.object({ role: z.enum(["user", "assistant"]), content: z.string().max(MAX_MESSAGE_CHARS) }))
    .min(1)
    .max(MAX_MESSAGES),
});

/** The model to run the agent on. `AGENT_MODEL` can name any model OpenRouter offers. */
function pickModel(): { model: LanguageModel; name: string } | { missing: string } {
  const apiKey = process.env.OPENROUTER_API_KEY;
  if (!apiKey) return { missing: "Add OPENROUTER_API_KEY to the .env file and restart to chat with an agent." };
  const name = process.env.AGENT_MODEL || DEFAULT_MODEL;
  return { model: createOpenRouter({ apiKey })(name), name };
}

export function GET(request: NextRequest) {
  const refused = refusal(request);
  if (refused) return refused;
  const picked = pickModel();
  return Response.json("missing" in picked ? { enabled: false, reason: picked.missing } : { enabled: true, model: picked.name });
}

export async function POST(request: NextRequest) {
  const refused = refusal(request);
  if (refused) return refused;
  const picked = pickModel();
  if ("missing" in picked) return Response.json({ error: { code: "no_model_key", message: picked.missing } }, { status: 503 });
  const conversation = Conversation.safeParse(await request.json().catch(() => undefined));
  if (!conversation.success) {
    return Response.json({ error: { code: "invalid_request", message: `this is not a conversation the demo can run: ${z.prettifyError(conversation.error)}` } }, { status: 400 });
  }
  try {
    const tools = await tenantFrom(request).tools();
    const result = await generateText({
      model: picked.model,
      instructions: INSTRUCTIONS,
      messages: conversation.data.messages,
      tools,
      stopWhen: isStepCount(MAX_STEPS),
    });
    const calls = result.steps.flatMap((step) =>
      step.toolCalls.map((call) => ({
        name: call.toolName,
        input: call.input,
        output: step.toolResults.find((toolResult) => toolResult.toolCallId === call.toolCallId)?.output,
      })),
    );
    return Response.json({ text: result.text, calls });
  } catch (error) {
    return errorResponse(error);
  }
}
