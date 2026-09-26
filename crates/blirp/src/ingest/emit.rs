//! Seq allocation and event construction shared by adapters.
//!
//! Seqs must be deterministic for the same source content (re-reading after
//! truncation dedupes on `(session_id, seq)`) and increase with arrival (the
//! distiller consumes events past `distilled_through_seq`). Line-oriented
//! sources use `line_index * SEQ_PER_LINE + n`; sources without stable lines
//! keep a counter in their cursor.

use super::text;
use super::{EventSink, NormEvent, Result};
use blirp_core::model::EventKind;
use serde_json::{Value, json};

/// Events one source line may produce; extra ones are dropped with a warning.
pub const SEQ_PER_LINE: i64 = 1024;

pub struct Emit<'a> {
    sink: &'a mut dyn EventSink,
    asid: &'a str,
    next: i64,
    end: i64,
    overflowed: bool,
}

impl<'a> Emit<'a> {
    /// Seqs for events of source line `line` (0-based).
    pub fn line(sink: &'a mut dyn EventSink, asid: &'a str, line: u64) -> Self {
        let base = i64::try_from(line).unwrap_or(i64::MAX / SEQ_PER_LINE - 1) * SEQ_PER_LINE;
        Self {
            sink,
            asid,
            next: base,
            end: base + SEQ_PER_LINE,
            overflowed: false,
        }
    }

    /// Seqs from a counter the adapter keeps in its cursor.
    pub fn counter(sink: &'a mut dyn EventSink, asid: &'a str, next: i64) -> Self {
        Self {
            sink,
            asid,
            next,
            end: i64::MAX,
            overflowed: false,
        }
    }

    /// The next unused seq (store it back into the cursor for counters).
    pub fn next_seq(&self) -> i64 {
        self.next
    }

    pub fn sink(&mut self) -> &mut dyn EventSink {
        self.sink
    }

    pub fn push(
        &mut self,
        ts: Option<i64>,
        kind: EventKind,
        text: String,
        meta: Option<Value>,
    ) -> Result<()> {
        if self.next >= self.end {
            if !self.overflowed {
                self.overflowed = true;
                self.sink
                    .warn("more events from one line than seqs available; extra dropped");
            }
            return Ok(());
        }
        let seq = self.next;
        self.next += 1;
        self.sink.event(
            self.asid,
            NormEvent {
                seq,
                ts,
                kind,
                text,
                meta,
            },
        )
    }

    /// A text event; blank text is skipped.
    pub fn text(
        &mut self,
        ts: Option<i64>,
        kind: EventKind,
        text: &str,
        meta: Option<Value>,
    ) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.push(ts, kind, text.to_string(), meta)
    }

    /// A tool call, plus one `file_edit` event per file it writes.
    pub fn tool_call(
        &mut self,
        ts: Option<i64>,
        name: &str,
        id: Option<&str>,
        args: &Value,
    ) -> Result<()> {
        self.push(
            ts,
            EventKind::ToolCall,
            text::tool_call_text(name, args),
            Some(json!({ "tool": name, "id": id, "args": args })),
        )?;
        for path in text::edited_paths(name, args) {
            self.file_edit(ts, &path, Some(name), id)?;
        }
        Ok(())
    }

    pub fn file_edit(
        &mut self,
        ts: Option<i64>,
        path: &str,
        tool: Option<&str>,
        id: Option<&str>,
    ) -> Result<()> {
        self.push(
            ts,
            EventKind::FileEdit,
            format!("edited {path}"),
            Some(json!({ "path": path, "tool": tool, "id": id })),
        )
    }

    /// A tool result; `extra` keys are merged into the meta.
    pub fn tool_result(
        &mut self,
        ts: Option<i64>,
        name: Option<&str>,
        id: Option<&str>,
        output: &str,
        is_error: bool,
        extra: Option<Value>,
    ) -> Result<()> {
        let mut meta = json!({ "tool": name, "id": id, "is_error": is_error });
        if let (Some(Value::Object(add)), Value::Object(m)) = (extra, &mut meta) {
            m.extend(add);
        }
        let body = if output.trim().is_empty() {
            if is_error { "(error)" } else { "(no output)" }.to_string()
        } else {
            output.to_string()
        };
        self.push(ts, EventKind::ToolResult, body, Some(meta))
    }
}
