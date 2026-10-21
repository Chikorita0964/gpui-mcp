# Reasoning-leak guard
Requests are refused (HTTP 200, `stop_reason: "refusal"`, category `reasoning_extraction`; retrying never helps) when
they ask the model to show its reasoning, or carry such wording or recorded model notes in context.
- Prompts, skills, tool descriptions and schemas ask for the conclusion and its evidence, never for reasoning in the
  output.
- Never print such wording or notes, and never type those words yourself, not even in a search pattern: run
  `scripts/leakscan` (don't read it) and follow its output, then rewrite the flagged files whole.
