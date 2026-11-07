//! Compact tool output previews. Keep three screen rows and report hidden logical lines below
//! them; a partially displayed logical line counts as hidden. Full output belongs in the transcript.
//! Preserve logical source ranges and hyperlinks while bounding work before display wrapping.

use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::live_wrap::take_prefix_by_width;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::LogicalLineSource;
use crate::terminal_hyperlinks::TerminalHyperlink;
use crate::terminal_hyperlinks::remap_source_wrapped_line;
use crate::ui_consts::transcript_hint;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_line_with_source;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use textwrap::WordSplitter;

const PREVIEW_LINES: usize = 3;
const MAX_PREVIEW_LINE_BYTES: usize = 16 * 1024;

const WAKEUP_HINT: &str = "Ask to list pending wakeups or cancel an ID. Pending work needs a live client with this conversation open. Leaving or closing the last one cancels pending work; resuming it does not restore jobs.";

/// Render immutable operation receipts, never a client-side view of the registry.
pub(crate) fn wakeup_receipt_cell(
    name: &str,
    namespace: Option<&str>,
    output: &codex_protocol::models::FunctionCallOutputBody,
) -> Option<crate::history_cell::PrefixedWrappedHistoryCell> {
    if namespace != Some("clock")
        || !matches!(name, "schedule_wakeup" | "list_wakeups" | "cancel_wakeup")
    {
        return None;
    }
    let text = output.to_text().unwrap_or_default();
    let value = serde_json::from_str::<serde_json::Value>(&text).ok();
    let mut lines = Vec::new();
    let rendered = value.as_ref().is_some_and(|value| match name {
        "schedule_wakeup" => append_wakeup_receipt(&mut lines, value),
        "list_wakeups" => {
            let Some(wakeups) = value.get("wakeups").and_then(serde_json::Value::as_array) else {
                return false;
            };
            lines.push(Line::from("Pending wakeups at this check".bold()));
            if wakeups.is_empty() {
                lines.push(Line::from("No pending wakeups."));
            }
            for wakeup in wakeups {
                if !append_wakeup_receipt(&mut lines, wakeup) {
                    return false;
                }
            }
            true
        }
        "cancel_wakeup" => {
            let Some(id) = value.get("task_id").and_then(serde_json::Value::as_str) else {
                return false;
            };
            let status = match value.get("status").and_then(serde_json::Value::as_str) {
                Some("cancelled") => "Wakeup cancelled",
                Some("not_pending") => "Wakeup not pending (already started or unknown ID)",
                _ => return false,
            };
            lines.push(Line::from(status.bold()));
            lines.push(Line::from(format!("ID: {id}")));
            true
        }
        _ => false,
    });
    if !rendered {
        lines.clear();
        lines.push(Line::from("Wakeup request failed".bold()));
        if value.is_none() && !text.trim().is_empty() {
            lines.extend(text.lines().map(|line| Line::from(line.to_owned())));
        } else {
            lines.push(Line::from("The wakeup receipt could not be read. Ask to list pending wakeups before retrying."));
        }
        lines.push(Line::from(
            "Check the delay, instruction and task ID, or cancel a pending wakeup and retry.",
        ));
    }
    lines.push(Line::from(WAKEUP_HINT.dim()));
    Some(crate::history_cell::PrefixedWrappedHistoryCell::new(
        lines,
        Line::from("◦ ".cyan()),
        Line::from("  "),
    ))
}

fn append_wakeup_receipt(lines: &mut Vec<Line<'static>>, value: &serde_json::Value) -> bool {
    let Some(id) = value.get("task_id").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(due) = value.get("due_at").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(instruction) = value.get("instruction").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let status = match value.get("status").and_then(serde_json::Value::as_str) {
        Some("scheduled") => "Wakeup scheduled",
        Some("due") => "Wakeup due — waiting for the current turn to finish",
        _ => return false,
    };
    lines.push(Line::from(status.bold()));
    lines.push(Line::from(format!("ID: {id}")));
    lines.push(Line::from(format!("Due: {due}")));
    lines.extend(instruction.lines().map(|line| Line::from(line.to_owned())));
    true
}

/// A framework marker, distinct from assistant prose and from a new user request.
pub(crate) fn wakeup_started_cell(
    task_id: &str,
    instruction: &str,
) -> crate::history_cell::PrefixedWrappedHistoryCell {
    let mut lines = vec![
        Line::from("Wakeup started".bold()),
        Line::from(format!("ID: {task_id}")),
    ];
    lines.extend(instruction.lines().map(|line| Line::from(line.to_owned())));
    crate::history_cell::PrefixedWrappedHistoryCell::new(
        lines,
        Line::from("◦ ".cyan()),
        Line::from("  "),
    )
}

/// Render a bounded preview with source ranges retained for transcript selection and copying.
pub(crate) fn tool_output_hyperlink_preview<'a>(
    lines: impl IntoIterator<Item = Line<'a>>,
    width: usize,
    total_lines: usize,
) -> Vec<HyperlinkLine> {
    let mut preview = ToolOutputPreview::new(width, /*omitted*/ 0);
    let mut consumed = 0;
    for line in lines.into_iter().take(PREVIEW_LINES) {
        preview.push_line(line);
        consumed += 1;
    }
    preview.omitted += total_lines.saturating_sub(consumed);
    preview.finish_hyperlink_lines()
}

pub(crate) struct ToolOutputPreview {
    lines: Vec<HyperlinkLine>,
    width: usize,
    omitted: usize,
    full: bool,
}

impl ToolOutputPreview {
    pub(crate) fn new(width: usize, omitted: usize) -> Self {
        Self {
            lines: Vec::new(),
            width: width.max(/*other*/ 1),
            omitted,
            full: false,
        }
    }

    pub(crate) fn push_line(&mut self, line: Line<'_>) {
        self.push_rendered_line(line, /*source*/ None, Vec::new());
    }

    pub(crate) fn push_hyperlink_line(&mut self, line: HyperlinkLine) {
        self.push_rendered_line(line.line, line.source, line.hyperlinks);
    }

    fn push_rendered_line(
        &mut self,
        line: Line<'_>,
        source: Option<LogicalLineSource>,
        hyperlinks: Vec<TerminalHyperlink>,
    ) {
        let remaining = PREVIEW_LINES - self.lines.len();
        if self.full || remaining == 0 {
            self.omitted += 1;
            return;
        }
        // Bound the input before wrapping: a single tool-result line can be megabytes long.
        // Keep one extra row so cutting a word cannot change the last visible row's wrapping.
        let mut columns = self.width.saturating_mul(remaining + 1);
        // Zero-width characters still cost bytes and work, even within one grapheme.
        let mut bytes = MAX_PREVIEW_LINE_BYTES;
        let mut bounded = Line::default().style(line.style);
        let mut truncated = false;
        for span in &line.spans {
            let text = &span.content[..span.content.floor_char_boundary(bytes)];
            let (prefix, rest, used) = take_prefix_by_width(text, columns);
            bytes -= prefix.len();
            bounded.push_span(Span::styled(prefix.to_owned(), span.style));
            columns = columns.saturating_sub(used);
            if !rest.is_empty() || text.len() < span.content.len() {
                truncated = true;
                break;
            }
        }
        // Hard-wrap even URL tokens here so the preview cannot exceed its row budget, while
        // remapping each visible fragment to the original hyperlink destination and source.
        let bounded = HyperlinkLine {
            line: bounded,
            hyperlinks,
            source,
        };
        let wrapped = word_wrap_line_with_source(
            &bounded.line,
            RtOptions::new(self.width).word_splitter(WordSplitter::NoHyphenation),
        );
        if truncated || wrapped.len() > remaining {
            self.omitted += 1;
            self.full = true;
        }
        self.lines.extend(remap_source_wrapped_line(
            &bounded,
            wrapped.into_iter().take(remaining).collect(),
        ));
    }

    pub(crate) fn finish_hyperlink_lines(mut self) -> Vec<HyperlinkLine> {
        if self.omitted > 0 {
            let omitted = self.omitted;
            let unit = if omitted == 1 { "line" } else { "lines" };
            self.lines.push(
                truncate_line_with_ellipsis_if_overflow(
                    format!("+{omitted} {unit} ({})", transcript_hint())
                        .dim()
                        .into(),
                    self.width,
                )
                .into(),
            );
        }
        self.lines
    }
}

#[cfg(test)]
#[path = "tool_output_tests.rs"]
mod tests;
