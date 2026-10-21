# Reasoning-leak guard
Some prompt wording, and recorded model notes in context, get requests refused (HTTP 200, `stop_reason: "refusal"`,
category `reasoning_extraction`; retrying never helps).
- Prompts, skills, tool descriptions and schemas ask for the conclusion and its evidence.
- Keep recorded model notes out of every request, prompt and output; pass on results, not notes.
- Before reading or editing prompts, transcripts or logs, run `scripts/leakscan` (don't read it) and follow its
  output; rewrite flagged files whole. Never type the flagged words yourself, not even in a search pattern.
