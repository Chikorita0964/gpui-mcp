# Reasoning-leak guard
Some prompt wording, and recorded model notes in context, get requests refused (category `reasoning_extraction`;
retrying never helps).
- Prompts, skills, tool descriptions and schemas ask for the conclusion and its evidence.
- Keep recorded model notes out of every request, prompt and output; pass on results, not notes.
- Before reading or editing prompts, transcripts or logs, run `scripts/leakscan` (don't read it) and follow its
  output.
