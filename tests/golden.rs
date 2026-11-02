use choregos::runtime::cli::replay_file;
use std::{fmt::Write as _, fs, path::Path};

#[test]
fn golden_replay_corpus() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut cases: Vec<_> = fs::read_dir(root)
        .expect("read golden corpus")
        .map(|entry| entry.expect("read golden case").path())
        .filter(|path| path.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "golden corpus has no cases");
    let update = std::env::var("UPDATE_GOLDEN").as_deref() == Ok("1");
    let mut mismatches = String::new();
    for case in cases {
        let name = case.file_name().unwrap().to_string_lossy();
        let mut actual = Vec::new();
        replay_file(
            &case.join("config.toml"),
            &case.join("trace.jsonl"),
            &mut actual,
        )
        .unwrap_or_else(|error| panic!("{name}: replay failed: {error:#}"));
        assert!(!actual.is_empty(), "{name}: replay produced no decisions");
        let expected_path = case.join("expected.jsonl");
        if update {
            fs::write(expected_path, actual).expect("update golden decisions");
        } else {
            let expected = fs::read(expected_path)
                .unwrap_or_else(|error| panic!("{name}: read expected decisions: {error}"));
            if expected != actual {
                writeln!(mismatches, "{}", difference(&name, &expected, &actual)).unwrap();
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "golden replay mismatch:\n{mismatches}"
    );
}

fn difference(name: &str, expected: &[u8], actual: &[u8]) -> String {
    let expected_text = String::from_utf8_lossy(expected);
    let actual_text = String::from_utf8_lossy(actual);
    let expected_lines: Vec<_> = expected_text.lines().collect();
    let actual_lines: Vec<_> = actual_text.lines().collect();
    let first = expected_lines
        .iter()
        .zip(&actual_lines)
        .position(|(expected, actual)| expected != actual)
        .unwrap_or(expected_lines.len().min(actual_lines.len()));
    let mut diff = format!(
        "--- {name}/expected.jsonl ({} lines, {} bytes)\n+++ {name}/actual ({} lines, {} bytes)\n@@ first difference at line {} @@\n",
        expected_lines.len(),
        expected.len(),
        actual_lines.len(),
        actual.len(),
        first + 1,
    );
    for index in
        first.saturating_sub(2)..(first + 3).min(expected_lines.len().max(actual_lines.len()))
    {
        match (expected_lines.get(index), actual_lines.get(index)) {
            (Some(expected), Some(actual)) if expected == actual => {
                writeln!(diff, " {}: {expected}", index + 1).unwrap()
            }
            (expected, actual) => {
                if let Some(line) = expected {
                    writeln!(diff, "-{}: {line}", index + 1).unwrap();
                }
                if let Some(line) = actual {
                    writeln!(diff, "+{}: {line}", index + 1).unwrap();
                }
            }
        }
    }
    if expected_lines == actual_lines {
        diff.push_str("Line contents match; newline/byte encoding differs.\n");
    }
    diff
}
