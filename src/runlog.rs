//! Per-run timeline.
//!
//! One [`RunLog`] records, with timestamps, everything a purchase attempt (or a
//! stuck episode) did **or tried to do** — including the branches that failed.
//! It renders twice: as the human `.log` file kept next to the recording, and as
//! a compact block for a Discord embed. The run ID it carries is the same string
//! that names the footage directory and the `purchases.jsonl` line, so a log can
//! always be tied back to the recording it explains.

use std::time::Instant;

use parking_lot::Mutex;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;

/// Severity of a timeline line; only changes the marker in the rendered log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Step,
    Note,
    Warn,
    Fail,
}

impl Level {
    fn tag(self) -> &'static str {
        match self {
            Level::Step => "",
            Level::Note => "note ",
            Level::Warn => "warn ",
            Level::Fail => "FAIL ",
        }
    }
}

struct Entry {
    elapsed_ms: u128,
    wall: String,
    level: Level,
    stage: String,
    msg: String,
}

/// A timestamped record of one run, keyed by its run ID.
pub struct RunLog {
    id: String,
    start: Instant,
    started: String,
    header: Mutex<Vec<(String, String)>>,
    entries: Mutex<Vec<Entry>>,
}

impl RunLog {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            start: Instant::now(),
            started: wall_stamp(),
            header: Mutex::new(Vec::new()),
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Milliseconds since the run started.
    pub fn elapsed_ms(&self) -> u128 {
        self.start.elapsed().as_millis()
    }

    /// Add a header line (`key: value`) shown under the run ID.
    pub fn context(&self, key: &str, value: impl Into<String>) {
        self.header.lock().push((key.to_string(), value.into()));
    }

    fn push(&self, level: Level, stage: &str, msg: impl Into<String>) {
        let entry = Entry {
            elapsed_ms: self.start.elapsed().as_millis(),
            wall: wall_clock(),
            level,
            stage: stage.to_string(),
            msg: msg.into(),
        };
        self.entries.lock().push(entry);
    }

    pub fn step(&self, stage: &str, msg: impl Into<String>) {
        self.push(Level::Step, stage, msg);
    }
    pub fn note(&self, stage: &str, msg: impl Into<String>) {
        self.push(Level::Note, stage, msg);
    }
    pub fn warn(&self, stage: &str, msg: impl Into<String>) {
        self.push(Level::Warn, stage, msg);
    }
    pub fn fail(&self, stage: &str, msg: impl Into<String>) {
        self.push(Level::Fail, stage, msg);
    }

    /// The full `.log` file: header context, then one line per step.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("Elorin bot run {}\n", self.id));
        out.push_str(&format!("started {}\n", self.started));
        for (k, v) in self.header.lock().iter() {
            out.push_str(&format!("{k}: {v}\n"));
        }
        out.push('\n');
        for e in self.entries.lock().iter() {
            out.push_str(&full_line(e));
            out.push('\n');
        }
        out
    }

    /// A compact version for a Discord embed code block, capped at `max_chars`.
    pub fn timeline(&self, max_chars: usize) -> String {
        let entries = self.entries.lock();
        // Leave room for the truncation notice.
        let budget = max_chars.saturating_sub(80);
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0usize;
        for e in entries.iter() {
            let line = compact_line(e);
            if used + line.len() + 1 > budget {
                let remaining = entries.len() - lines.len();
                lines.push(format!("… {remaining} more line(s) — see the attached .log"));
                break;
            }
            used += line.len() + 1;
            lines.push(line);
        }
        if lines.is_empty() {
            lines.push("(no steps recorded)".to_string());
        }
        lines.join("\n")
    }
}

fn full_line(e: &Entry) -> String {
    format!(
        "[{:>7} ms] {}  {}{:<9} {}",
        e.elapsed_ms,
        e.wall,
        e.level.tag(),
        e.stage,
        e.msg
    )
}

fn compact_line(e: &Entry) -> String {
    let short = &e.wall[..e.wall.len().min(8)];
    format!(
        "[{:>6}ms] {} {}{} {}",
        e.elapsed_ms,
        short,
        e.level.tag(),
        e.stage,
        e.msg
    )
}

fn local_now() -> SYSTEMTIME {
    unsafe { GetLocalTime() }
}

/// `HH:MM:SS.mmm`, local time.
fn wall_clock() -> String {
    let t = local_now();
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

/// `YYYY-MM-DD HH:MM:SS.mmm`, local time.
fn wall_stamp() -> String {
    let t = local_now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_lists_steps_in_order_with_header() {
        let r = RunLog::new("42");
        r.context("item", "Ruby");
        r.step("start", "attempt for Ruby");
        r.warn("open", "retrying Open");
        let text = r.render();
        assert!(text.contains("Elorin bot run 42"), "{text}");
        assert!(text.contains("item: Ruby"), "{text}");
        let a = text.find("attempt for Ruby").unwrap();
        let b = text.find("retrying Open").unwrap();
        assert!(a < b, "steps must be chronological");
        assert!(text.contains("warn open"), "{text}");
    }

    #[test]
    fn timeline_truncates_to_the_budget() {
        let r = RunLog::new("1");
        for i in 0..500 {
            r.step("step", format!("line {i}"));
        }
        let t = r.timeline(500);
        assert!(t.len() <= 560, "timeline too long: {} chars", t.len());
        assert!(t.contains("more line(s)"), "{t}");
    }

    #[test]
    fn empty_timeline_is_well_formed() {
        let r = RunLog::new("7");
        assert!(r.timeline(100).contains("no steps recorded"));
    }
}
