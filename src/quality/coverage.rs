//! Coverage parsers (AIR-6): one small function per supported tool format, all
//! producing the same normalized `Coverage` struct so `test_report` and later stages
//! never need to know which tool actually produced the numbers.
//!
//! Adding a new format means adding one `parse_*` function here and a match arm in
//! `parse` -- nothing else in the crate should ever need to know a format-specific
//! shape.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageFormat {
    LlvmCov,
    Lcov,
    Cobertura,
    Jacoco,
    /// No coverage tool configured (or an unrecognized `format:` value) -- always
    /// degrades to `Coverage::not_measured()`, never fails the cycle.
    None,
}

impl CoverageFormat {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "llvm-cov" => CoverageFormat::LlvmCov,
            "lcov" => CoverageFormat::Lcov,
            "cobertura" => CoverageFormat::Cobertura,
            "jacoco" => CoverageFormat::Jacoco,
            _ => CoverageFormat::None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileCoverage {
    pub path: String,
    pub lines_covered: u64,
    pub lines_total: u64,
}

/// Normalized coverage result -- the only shape `test_report`/the dashboard/`AIR-9`'s
/// traceability manifest ever deal with, regardless of which tool produced it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Coverage {
    /// `false` when no coverage tool ran (absent config, `format: none`, or the tool's
    /// own run/parse failed) -- the "not measured" degrade path. `min_line_percent`
    /// gating must treat this as "nothing to check", never as 0%.
    pub measured: bool,
    pub lines_covered: u64,
    pub lines_total: u64,
    pub files: Vec<FileCoverage>,
}

impl Coverage {
    pub fn not_measured() -> Self {
        Coverage {
            measured: false,
            lines_covered: 0,
            lines_total: 0,
            files: Vec::new(),
        }
    }

    pub fn line_percent(&self) -> Option<f64> {
        if !self.measured || self.lines_total == 0 {
            None
        } else {
            Some(100.0 * self.lines_covered as f64 / self.lines_total as f64)
        }
    }

    fn from_files(files: Vec<FileCoverage>) -> Self {
        let lines_covered = files.iter().map(|f| f.lines_covered).sum();
        let lines_total = files.iter().map(|f| f.lines_total).sum();
        Coverage {
            measured: true,
            lines_covered,
            lines_total,
            files,
        }
    }
}

/// Parse `content` (already read from wherever the coverage command wrote it) as
/// `format`. Never returns `Err` for `CoverageFormat::None`; a malformed file in a
/// recognized format returns `Err` so the caller can log it, but the caller is expected
/// to fall back to `Coverage::not_measured()` rather than fail the cycle over it (see
/// `quality::run_coverage`).
pub fn parse(format: CoverageFormat, content: &str) -> Result<Coverage, String> {
    match format {
        CoverageFormat::None => Ok(Coverage::not_measured()),
        CoverageFormat::LlvmCov => parse_llvm_cov(content),
        CoverageFormat::Lcov => parse_lcov(content),
        CoverageFormat::Cobertura => parse_cobertura(content),
        CoverageFormat::Jacoco => parse_jacoco(content),
    }
}

/// `cargo llvm-cov --json` (LLVM source-based coverage export format):
/// `{"data": [{"files": [{"filename": "...", "summary": {"lines": {"count": N, "covered": M}}}]}]}`.
fn parse_llvm_cov(content: &str) -> Result<Coverage, String> {
    let root: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("invalid llvm-cov json: {e}"))?;
    let files_json = root
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .and_then(|d0| d0.get("files"))
        .and_then(|f| f.as_array())
        .ok_or_else(|| "llvm-cov json missing data[0].files".to_string())?;

    let files = files_json
        .iter()
        .filter_map(|f| {
            let path = f.get("filename")?.as_str()?.to_string();
            let lines = f.get("summary")?.get("lines")?;
            let total = lines.get("count")?.as_u64()?;
            let covered = lines.get("covered")?.as_u64()?;
            Some(FileCoverage {
                path,
                lines_covered: covered,
                lines_total: total,
            })
        })
        .collect();
    Ok(Coverage::from_files(files))
}

/// LCOV tracefile: `SF:<path>` starts a record, `LF:`/`LH:` give that file's totals,
/// `end_of_record` closes it. `DA:` per-line detail is intentionally not retained --
/// `FileCoverage` only needs per-file totals.
fn parse_lcov(content: &str) -> Result<Coverage, String> {
    let mut files = Vec::new();
    let mut current_path: Option<String> = None;
    let mut lf: u64 = 0;
    let mut lh: u64 = 0;

    for line in content.lines() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("SF:") {
            current_path = Some(path.trim().to_string());
            lf = 0;
            lh = 0;
        } else if let Some(v) = line.strip_prefix("LF:") {
            lf = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("LH:") {
            lh = v.trim().parse().unwrap_or(0);
        } else if line == "end_of_record"
            && let Some(path) = current_path.take()
        {
            files.push(FileCoverage {
                path,
                lines_covered: lh,
                lines_total: lf,
            });
        }
    }
    if files.is_empty() {
        return Err("lcov tracefile had no end_of_record entries".to_string());
    }
    Ok(Coverage::from_files(files))
}

/// Cobertura XML: `<class filename="..."><lines><line hits="N"/>...`. Per-file totals
/// come from counting `<line>` elements directly (not the class's own `line-rate`
/// attribute), so rounding in the tool's own summary never leaks into ours.
fn parse_cobertura(content: &str) -> Result<Coverage, String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut files = Vec::new();
    let mut current: Option<(String, u64, u64)> = None; // (path, covered, total)
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"class" => {
                        let filename = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == b"filename")
                            .and_then(|a| a.unescape_value().ok())
                            .map(|v| v.to_string());
                        if let Some(f) = filename {
                            current = Some((f, 0, 0));
                        }
                    }
                    b"line" if current.is_some() => {
                        let hits: u64 = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == b"hits")
                            .and_then(|a| a.unescape_value().ok())
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0);
                        if let Some((_, covered, total)) = current.as_mut() {
                            *total += 1;
                            if hits > 0 {
                                *covered += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"class" => {
                if let Some((path, covered, total)) = current.take() {
                    files.push(FileCoverage {
                        path,
                        lines_covered: covered,
                        lines_total: total,
                    });
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("invalid cobertura xml: {e}")),
        }
        buf.clear();
    }
    if files.is_empty() {
        return Err("cobertura xml had no <class filename=...> entries".to_string());
    }
    Ok(Coverage::from_files(files))
}

/// JaCoCo XML: `<package name="..."><sourcefile name="..."><counter type="LINE"
/// missed="M" covered="C"/>`. The file path is `<package name>/<sourcefile name>`.
fn parse_jacoco(content: &str) -> Result<Coverage, String> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(content);
    reader.config_mut().trim_text(true);

    let mut files = Vec::new();
    let mut package = String::new();
    let mut current_sourcefile: Option<String> = None;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"package" => {
                package = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.local_name().as_ref() == b"name")
                    .and_then(|a| a.unescape_value().ok())
                    .map(|v| v.to_string())
                    .unwrap_or_default();
            }
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"sourcefile" => {
                current_sourcefile = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.local_name().as_ref() == b"name")
                    .and_then(|a| a.unescape_value().ok())
                    .map(|v| v.to_string());
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"sourcefile" => {
                current_sourcefile = None;
            }
            Ok(Event::Empty(e)) if e.local_name().as_ref() == b"counter" => {
                let Some(name) = &current_sourcefile else {
                    continue;
                };
                let attrs: Vec<_> = e.attributes().flatten().collect();
                let ty = attrs
                    .iter()
                    .find(|a| a.key.local_name().as_ref() == b"type")
                    .and_then(|a| a.unescape_value().ok());
                if ty.as_deref() != Some("LINE") {
                    continue;
                }
                let get = |k: &[u8]| -> u64 {
                    attrs
                        .iter()
                        .find(|a| a.key.local_name().as_ref() == k)
                        .and_then(|a| a.unescape_value().ok())
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0)
                };
                let missed = get(b"missed");
                let covered = get(b"covered");
                let path = if package.is_empty() {
                    name.clone()
                } else {
                    format!("{package}/{name}")
                };
                files.push(FileCoverage {
                    path,
                    lines_covered: covered,
                    lines_total: covered + missed,
                });
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("invalid jacoco xml: {e}")),
        }
        buf.clear();
    }
    if files.is_empty() {
        return Err("jacoco xml had no <sourcefile>/<counter type=\"LINE\"> entries".to_string());
    }
    Ok(Coverage::from_files(files))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_recognizes_known_values_and_defaults_to_none() {
        assert_eq!(CoverageFormat::parse("llvm-cov"), CoverageFormat::LlvmCov);
        assert_eq!(CoverageFormat::parse("LCOV"), CoverageFormat::Lcov);
        assert_eq!(
            CoverageFormat::parse("cobertura"),
            CoverageFormat::Cobertura
        );
        assert_eq!(CoverageFormat::parse("jacoco"), CoverageFormat::Jacoco);
        assert_eq!(CoverageFormat::parse("none"), CoverageFormat::None);
        assert_eq!(CoverageFormat::parse("bogus"), CoverageFormat::None);
    }

    #[test]
    fn none_format_degrades_cleanly() {
        let cov = parse(CoverageFormat::None, "").unwrap();
        assert!(!cov.measured);
        assert_eq!(cov.line_percent(), None);
        assert!(cov.files.is_empty());
    }

    #[test]
    fn llvm_cov_json_parses_totals_and_per_file() {
        let json = r#"{
            "data": [{
                "files": [
                    {"filename": "src/a.rs", "summary": {"lines": {"count": 10, "covered": 8}}},
                    {"filename": "src/b.rs", "summary": {"lines": {"count": 5, "covered": 5}}}
                ]
            }]
        }"#;
        let cov = parse(CoverageFormat::LlvmCov, json).unwrap();
        assert!(cov.measured);
        assert_eq!(cov.lines_covered, 13);
        assert_eq!(cov.lines_total, 15);
        assert_eq!(cov.files.len(), 2);
        let a = cov.files.iter().find(|f| f.path == "src/a.rs").unwrap();
        assert_eq!(a.lines_covered, 8);
        assert_eq!(a.lines_total, 10);
    }

    #[test]
    fn llvm_cov_json_missing_shape_errors() {
        assert!(parse(CoverageFormat::LlvmCov, "{}").is_err());
    }

    #[test]
    fn lcov_parses_totals_and_per_file() {
        let lcov = "\
SF:src/a.rs
DA:1,1
DA:2,0
LF:2
LH:1
end_of_record
SF:src/b.rs
DA:1,1
LF:1
LH:1
end_of_record
";
        let cov = parse(CoverageFormat::Lcov, lcov).unwrap();
        assert!(cov.measured);
        assert_eq!(cov.lines_covered, 2);
        assert_eq!(cov.lines_total, 3);
        assert_eq!(cov.files.len(), 2);
    }

    #[test]
    fn lcov_without_records_errors() {
        assert!(parse(CoverageFormat::Lcov, "not lcov at all").is_err());
    }

    #[test]
    fn cobertura_xml_counts_lines_from_hits() {
        let xml = r#"<?xml version="1.0"?>
<coverage>
  <packages>
    <package name="pkg">
      <classes>
        <class name="Foo" filename="src/foo.rs" line-rate="0.5">
          <lines>
            <line number="1" hits="1"/>
            <line number="2" hits="0"/>
          </lines>
        </class>
        <class name="Bar" filename="src/bar.rs" line-rate="1.0">
          <lines>
            <line number="1" hits="3"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;
        let cov = parse(CoverageFormat::Cobertura, xml).unwrap();
        assert!(cov.measured);
        assert_eq!(cov.lines_covered, 2);
        assert_eq!(cov.lines_total, 3);
        assert_eq!(cov.files.len(), 2);
        let foo = cov.files.iter().find(|f| f.path == "src/foo.rs").unwrap();
        assert_eq!(foo.lines_covered, 1);
        assert_eq!(foo.lines_total, 2);
    }

    #[test]
    fn cobertura_without_classes_errors() {
        assert!(parse(CoverageFormat::Cobertura, "<coverage></coverage>").is_err());
    }

    #[test]
    fn jacoco_xml_parses_per_file_line_counters() {
        let xml = r#"<?xml version="1.0"?>
<report name="example">
  <package name="com/example">
    <sourcefile name="Foo.java">
      <counter type="INSTRUCTION" missed="1" covered="9"/>
      <counter type="LINE" missed="2" covered="8"/>
    </sourcefile>
  </package>
  <counter type="LINE" missed="2" covered="8"/>
</report>"#;
        let cov = parse(CoverageFormat::Jacoco, xml).unwrap();
        assert!(cov.measured);
        assert_eq!(cov.lines_covered, 8);
        assert_eq!(cov.lines_total, 10);
        assert_eq!(cov.files.len(), 1);
        assert_eq!(cov.files[0].path, "com/example/Foo.java");
    }

    #[test]
    fn jacoco_without_sourcefiles_errors() {
        assert!(parse(CoverageFormat::Jacoco, "<report></report>").is_err());
    }

    #[test]
    fn line_percent_none_when_not_measured_or_zero_total() {
        assert_eq!(Coverage::not_measured().line_percent(), None);
        let zero = Coverage::from_files(vec![]);
        assert_eq!(zero.line_percent(), None);
    }
}
