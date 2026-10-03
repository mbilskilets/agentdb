import { createOpenRouter } from "@openrouter/ai-sdk-provider";
import { generateText, isStepCount, type LanguageModel, type ModelMessage } from "ai";

import { errorResponse, tenantFrom } from "@/lib/db";

export const dynamic = "force-dynamic";

const MAX_STEPS = 12;
const INSTRUCTIONS = `You manage a small business database for the user through your tools.
Work out what exists with describe_database when unsure. Create or reshape tables with change_schema when asked.
When a tool returns an error, read it: it says how to fix the call. Fix the call and try again.
Never pass force: true unless the user has confirmed, after seeing how much data would be deleted.
When you are done, answer in one or two short sentences. Do not repeat the data back: the user sees the table update live.`;

const DEFAULT_MODEL = "anthropic/claude-sonnet-5.5";

/** The model to run the agent on. `AGENT_MODEL` can name any model OpenRouter offers. */
function pickModel(): { model: LanguageModel; name: string } | { missing: string } {
  const apiKey = process.env.OPENROUTER_API_KEY;
  if (!apiKey) return { missing: "Add OPENROUTER_API_KEY to the .env file and restart to chat with an agent." };
  const name = process.env.AGENT_MODEL || DEFAULT_MODEL;
  return { model: createOpenRouter({ apiKey })(name), name };
}

export function GET() {
  const picked = pickModel();
  return Response.json("missing" in picked ? { enabled: false, reason: picked.missing } : { enabled: true, model: picked.name });
}

export async function POST(request: Request) {
  const picked = pickModel();
  if ("missing" in picked) return Response.json({ error: { code: "no_model_key", message: picked.missing } }, { status: 503 });
  try {
    const { messages } = (await request.json()) as { messages: ModelMessage[] };
    const tools = await tenantFrom(request).tools();
    const result = await generateText({
      model: picked.model,
      instructions: INSTRUCTIONS,
      messages,
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
