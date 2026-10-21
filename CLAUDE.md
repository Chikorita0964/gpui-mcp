# Reasoning-leak guard
Requests are refused (HTTP 200, `stop_reason: "refusal"`, category `reasoning_extraction`; retrying never helps) when
they ask the model to show its reasoning, or carry such wording or recorded model notes in context.
- Prompts, skills, tool descriptions and schemas ask for the conclusion and its evidence, never for reasoning in the
  output. To expose it, enable `thinking` with `display: "summarized"`.
- Never print such wording or notes: locate files with `rg -l`, inspect structure with `jq keys` or `wc -c`, and
  rewrite them whole. Tools print only a path and a size.
- On a refusal: show `explanation`, don't retry, fix the wording.
