Synthetic, sanitized transcripts modeled on `docs/agent-formats.md` and the
agents' published formats. Nothing here is copied from a real machine.
Placeholders are filled in by `tests/ingest.rs`: `{{CWD}}` (a temp project
folder, JSON-escaped), `{{SECRET}}` (a fake token assembled at runtime so no
secret-shaped string is committed) and `{{NOW}}` (current time, RFC 3339).
