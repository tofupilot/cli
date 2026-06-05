//! Human-readable table formatting for command output (the non-`--json` path).

use chrono::{DateTime, Utc};
use comfy_table::{presets::NOTHING, ContentArrangement, Table};

/// Extract a value from a JSON object by dot-separated path (e.g. "unit.part.number").
pub fn json_path<'a>(value: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    let mut current = value;
    for key in path.split('.') {
        current = &current[key];
    }
    current
}

/// Format a JSON value as a display string.
pub fn format_value(value: &serde_json::Value, format: &str) -> String {
    match value {
        serde_json::Value::Null => "-".to_string(),
        serde_json::Value::String(s) => {
            if format == "relative" {
                if let Ok(dt) = s.parse::<DateTime<Utc>>() {
                    return format_relative(dt);
                }
            }
            s.clone()
        }
        serde_json::Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f == f.floor() {
                    format!("{}", f as i64)
                } else {
                    format!("{f}")
                }
            } else {
                n.to_string()
            }
        }
        serde_json::Value::Bool(b) => b.to_string(),
        _ => value.to_string(),
    }
}

/// Format a datetime as a relative time string (e.g. "2m ago", "3h ago").
fn format_relative(dt: DateTime<Utc>) -> String {
    let now = Utc::now();
    let diff = now.signed_duration_since(dt);

    let secs = diff.num_seconds();
    if secs < 0 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = diff.num_minutes();
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = diff.num_hours();
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = diff.num_days();
    if days < 30 {
        return format!("{days}d ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }
    format!("{}y ago", days / 365)
}

/// Truncate a string to max display width, adding "..." if truncated.
fn truncate(s: &str, max: usize) -> String {
    if max < 4 || s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max - 3).collect();
    format!("{truncated}...")
}

/// Column definition for table display.
pub struct Column {
    pub header: &'static str,
    pub path: &'static str,
    pub format: &'static str,
    pub width: usize,
    pub truncate: bool,
}

/// Print a list of JSON values as a table.
pub fn print_table(items: &[serde_json::Value], columns: &[Column]) {
    if items.is_empty() {
        eprintln!("No results.");
        return;
    }

    let mut table = Table::new();
    table.load_preset(NOTHING);
    table.set_content_arrangement(ContentArrangement::Dynamic);

    let headers: Vec<&str> = columns.iter().map(|c| c.header).collect();
    table.set_header(headers);

    for item in items {
        let row: Vec<String> = columns
            .iter()
            .map(|col| {
                let val = json_path(item, col.path);
                let s = format_value(val, col.format);
                if col.truncate && col.width > 0 {
                    truncate(&s, col.width)
                } else {
                    s
                }
            })
            .collect();
        table.add_row(row);
    }

    eprintln!("{table}");
}

/// Print a single JSON object as key-value pairs.
pub fn print_detail(value: &serde_json::Value, fields: &[(&str, &str, &str)]) {
    let max_label = fields
        .iter()
        .map(|(label, _, _)| label.len())
        .max()
        .unwrap_or(0);

    for (label, path, format) in fields {
        let val = json_path(value, path);
        if val.is_null() {
            continue;
        }
        let formatted = format_value(val, format);
        eprintln!("  {:<width$}  {}", label, formatted, width = max_label);
    }
}
