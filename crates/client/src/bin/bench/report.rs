//! The JSON report of a run and the Markdown tables made from reports.
//!
//! A report holds one entry per step (a connection count, a number of games, an endpoint...):
//! `headline` is the ordered set of figures the Markdown table shows, `detail` everything else.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::cli::Options;
use crate::ctx::epoch_ms;

/// Version of the report layout.
pub const REPORT_VERSION: u32 = 1;

/// One measured step.
#[derive(Debug)]
pub struct Step {
    /// What was measured (`1000 conns`, `pgn`...).
    pub name: String,
    /// Table figures, in column order: `(column, value)`.
    pub headline: Vec<(&'static str, Value)>,
    /// Every other detail.
    pub detail: Value,
}

impl Step {
    /// A step without figures yet.
    pub fn new(name: impl Into<String>) -> Step {
        Step { name: name.into(), headline: Vec::new(), detail: Value::Null }
    }

    /// Adds a table figure.
    pub fn figure(mut self, column: &'static str, value: impl Into<Value>) -> Step {
        self.headline.push((column, value.into()));
        self
    }

    /// Sets the details.
    pub fn detail(mut self, detail: Value) -> Step {
        self.detail = detail;
        self
    }
}

/// The report of a run.
pub fn build(opts: &Options, started_ms: u64, params: Value, steps: &[Step]) -> Value {
    let meta: Map<String, Value> = opts.meta.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
    let steps: Vec<Value> = steps
        .iter()
        .map(|s| {
            let headline: Map<String, Value> =
                s.headline.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
            json!({ "name": s.name, "headline": headline, "detail": s.detail })
        })
        .collect();
    json!({
        "tool": "scacelith-bench",
        "version": REPORT_VERSION,
        "scenario": opts.scenario.name(),
        "target": opts.target.name(),
        "label": opts.label,
        "meta": meta,
        "startedAtMs": started_ms,
        "finishedAtMs": epoch_ms(),
        "settings": {
            "addr": opts.addr.to_string(),
            "tls": !matches!(opts.tls, crate::cli::TlsMode::Plain),
            "tlsResume": opts.tls_resume,
            "warmupS": opts.warmup.as_secs_f64(),
            "durationS": opts.duration.as_secs_f64(),
            "serverPid": opts.server_pid,
        },
        "params": params,
        "steps": steps,
    })
}

fn cell(v: &Value) -> String {
    match v {
        Value::Null => "-".into(),
        Value::String(s) => s.replace('|', "\\|"),
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() => {
                let abs = f.abs();
                if abs >= 100.0 {
                    format!("{f:.0}")
                } else if abs >= 10.0 {
                    format!("{f:.1}")
                } else {
                    format!("{f:.2}")
                }
            }
            _ => n.to_string(),
        },
        other => other.to_string(),
    }
}

/// One row of a Markdown table: a run's step.
struct Row {
    label: String,
    step: String,
    figures: Map<String, Value>,
}

/// The Markdown table of one scenario.
struct Table {
    scenario: String,
    columns: Vec<String>,
    rows: Vec<Row>,
}

impl Table {
    fn render(&self, out: &mut String) {
        out.push_str(&format!("### {}\n\n| server | step |", self.scenario));
        for c in &self.columns {
            out.push_str(&format!(" {c} |"));
        }
        out.push_str("\n|---|---|");
        for _ in &self.columns {
            out.push_str("---:|");
        }
        out.push('\n');
        for row in &self.rows {
            out.push_str(&format!("| {} | {} |", row.label, row.step));
            for c in &self.columns {
                out.push_str(&format!(" {} |", row.figures.get(c).map(cell).unwrap_or_else(|| "-".into())));
            }
            out.push('\n');
        }
        out.push('\n');
    }
}

/// Markdown tables of report files: one table per scenario, one row per run and step.
pub fn table(files: &[PathBuf]) -> Result<String, String> {
    let mut tables: Vec<Table> = Vec::new();
    for path in files {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let report: Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a JSON report: {e}", path.display()))?;
        if report.get("tool").and_then(Value::as_str) != Some("scacelith-bench") {
            return Err(format!("{} is not a scacelith-bench report", path.display()));
        }
        let scenario = report["scenario"].as_str().unwrap_or("?").to_string();
        let label = report["label"].as_str().unwrap_or("?").to_string();
        let index = match tables.iter().position(|t| t.scenario == scenario) {
            Some(i) => i,
            None => {
                tables.push(Table { scenario, columns: Vec::new(), rows: Vec::new() });
                tables.len() - 1
            }
        };
        let table = &mut tables[index];
        for step in report["steps"].as_array().into_iter().flatten() {
            let figures = step["headline"].as_object().cloned().unwrap_or_default();
            for k in figures.keys() {
                if !table.columns.contains(k) {
                    table.columns.push(k.clone());
                }
            }
            let step = step["name"].as_str().unwrap_or("").to_string();
            table.rows.push(Row { label: label.clone(), step, figures });
        }
    }
    let mut out = String::new();
    for t in &tables {
        t.render(&mut out);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_and_tables() {
        assert_eq!(cell(&json!(1234.567)), "1235");
        assert_eq!(cell(&json!(12.345)), "12.3");
        assert_eq!(cell(&json!(1.2345)), "1.23");
        assert_eq!(cell(&json!(7)), "7");
        assert_eq!(cell(&json!("a|b")), "a\\|b");
        let dir = std::env::temp_dir().join(format!("bench-table-{}.json", std::process::id()));
        let report = json!({
            "tool": "scacelith-bench", "scenario": "rest", "label": "node24",
            "steps": [{"name": "info", "headline": {"req/s": 1000.0, "p99 ms": 2.5}}],
        });
        std::fs::write(&dir, report.to_string()).unwrap();
        let md = table(std::slice::from_ref(&dir)).unwrap();
        std::fs::remove_file(&dir).unwrap();
        assert!(md.contains("| server | step | req/s | p99 ms |"), "{md}");
        assert!(md.contains("| node24 | info | 1000 | 2.50 |"), "{md}");
    }
}
