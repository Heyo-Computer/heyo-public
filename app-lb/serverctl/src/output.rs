//! Rendering: the column writer, the machine-readable formats, and the unit
//! helpers shared by every command.
//!
//! Columns are space-padded rather than boxed, so output stays greppable and
//! `awk`-able — the same reason kubectl does it.

use anyhow::Result;
use clap::ValueEnum;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum OutputFormat {
    /// Columns (the default).
    Table,
    /// Columns, plus the ones that don't fit the default view.
    Wide,
    /// The server's JSON, unmodified.
    Json,
    /// The server's JSON, as YAML.
    Yaml,
    /// Just `deployment/<id>` lines, for piping into xargs.
    Name,
}

impl OutputFormat {
    /// Whether this format prints the server's payload verbatim, in which case
    /// a command should not also print a table or a status line.
    pub fn is_machine(self) -> bool {
        matches!(self, Self::Json | Self::Yaml | Self::Name)
    }

    pub fn is_wide(self) -> bool {
        self == Self::Wide
    }
}

/// Print a payload in a machine-readable format. `names` supplies the
/// `TYPE/NAME` lines for `-o name`, which JSON alone can't imply.
pub fn emit(value: &Value, format: OutputFormat, names: &[String]) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(value)?),
        OutputFormat::Yaml => print!("{}", serde_yaml::to_string(value)?),
        OutputFormat::Name => {
            for n in names {
                println!("{n}");
            }
        }
        OutputFormat::Table | OutputFormat::Wide => unreachable!("not a machine format"),
    }
    Ok(())
}

pub struct Table {
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Left margin, for a table nested under a `describe` section.
    indent: usize,
}

impl Table {
    pub fn new<I, S>(headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            headers: headers.into_iter().map(Into::into).collect(),
            rows: Vec::new(),
            indent: 0,
        }
    }

    /// Indent every line, including the header row.
    pub fn indented<I, S>(headers: I, indent: usize) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            indent,
            ..Self::new(headers)
        }
    }

    pub fn row<I, S>(&mut self, cells: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.rows.push(cells.into_iter().map(Into::into).collect());
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn print(&self) {
        let mut widths: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                if i < widths.len() {
                    widths[i] = widths[i].max(cell.chars().count());
                }
            }
        }
        print_row(&self.headers, &widths, self.indent);
        for row in &self.rows {
            print_row(row, &widths, self.indent);
        }
    }
}

fn print_row(cells: &[String], widths: &[usize], indent: usize) {
    let mut line = " ".repeat(indent);
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            line.push_str("   ");
        }
        line.push_str(cell);
        // The last column is never padded, so lines have no trailing blanks.
        if i + 1 < cells.len() {
            let pad = widths.get(i).copied().unwrap_or(0);
            for _ in cell.chars().count()..pad {
                line.push(' ');
            }
        }
    }
    println!("{}", line.trim_end());
}

/// A section heading; its contents follow as [`field`]s, indented under it.
pub fn section(title: &str) {
    println!("{title}:");
}

/// A key/value line inside a section.
pub fn field(key: &str, value: impl AsRef<str>) {
    println!("  {:<24} {}", format!("{key}:"), value.as_ref());
}

/// A key/value line at the top level, above any section — `describe`'s
/// Name/Kind header.
pub fn top_field(key: &str, value: impl AsRef<str>) {
    println!("{:<26} {}", format!("{key}:"), value.as_ref());
}

// -- Unit helpers ----------------------------------------------------------

pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub fn opt_bytes(n: Option<u64>) -> String {
    n.map(bytes).unwrap_or_else(|| "—".into())
}

/// Compact duration, kubectl's AGE style: `12s`, `4m30s`, `3h12m`, `6d4h`.
pub fn duration(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let (m, s) = (secs / 60, secs % 60);
    if m < 60 {
        return if s == 0 { format!("{m}m") } else { format!("{m}m{s}s") };
    }
    let (h, m) = (m / 60, m % 60);
    if h < 24 {
        return if m == 0 { format!("{h}h") } else { format!("{h}h{m}m") };
    }
    let (d, h) = (h / 24, h % 24);
    if h == 0 { format!("{d}d") } else { format!("{d}d{h}h") }
}

pub fn percent(v: f64) -> String {
    format!("{v:.1}%")
}

pub fn opt_percent(v: Option<f64>) -> String {
    v.map(percent).unwrap_or_else(|| "—".into())
}

/// A ratio (0.0–1.0+) as a percentage. `None` means "no capacity to divide by",
/// which is not the same as zero load.
pub fn ratio_percent(v: Option<f64>) -> String {
    v.map(|r| format!("{:.0}%", r * 100.0)).unwrap_or_else(|| "—".into())
}

pub fn millis(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.2}s", v / 1000.0)
    } else if v >= 10.0 {
        format!("{v:.0}ms")
    } else {
        format!("{v:.1}ms")
    }
}

pub fn opt_str(v: Option<&str>) -> String {
    v.unwrap_or("—").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_scale_to_a_readable_unit() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(bytes(700 * 1024 * 1024), "700 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn durations_use_two_units_at_most() {
        assert_eq!(duration(9), "9s");
        assert_eq!(duration(90), "1m30s");
        assert_eq!(duration(3600), "1h");
        assert_eq!(duration(3660), "1h1m");
        assert_eq!(duration(86_400 * 6 + 3600 * 4), "6d4h");
    }

    #[test]
    fn absent_gauges_are_a_dash_not_a_zero() {
        assert_eq!(opt_bytes(None), "—");
        assert_eq!(ratio_percent(None), "—");
        assert_eq!(ratio_percent(Some(0.0)), "0%");
    }

    #[test]
    fn latency_switches_unit_at_a_second() {
        assert_eq!(millis(4.25), "4.2ms");
        assert_eq!(millis(250.0), "250ms");
        assert_eq!(millis(1500.0), "1.50s");
    }
}
