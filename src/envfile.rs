//! `env-file` parsing: the `KEY=VALUE` pairs a service loads at session start.
//!
//! The syntax is the standard dotenv one (comments, `export KEY=VALUE`, single
//! and double quotes, escapes, `$VAR` substitution), implemented by `dotenvy`.
//! Only its iterator API is used, so agproc's own environment is never touched:
//! the values belong to the child, exactly like the `env` table.
//!
//! Failure is loud, as everywhere else in the config: a `.env` an agent mistyped
//! must stop the start instead of silently running with half the variables.

use anyhow::{Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::Path;

/// Read `KEY=VALUE` pairs from one dotenv file.
///
/// A key declared twice is an error rather than a silent first-wins/last-wins
/// choice: in a single file that is always an editing accident.
pub fn load(path: &Path) -> Result<BTreeMap<String, String>> {
    let items = dotenvy::from_path_iter(path)
        .map_err(|err| anyhow!("cannot read env-file {path}: {err}", path = path.display()))?;
    collect(path, items)
}

fn collect<I>(path: &Path, items: I) -> Result<BTreeMap<String, String>>
where
    I: Iterator<Item = dotenvy::Result<(String, String)>>,
{
    let mut vars = BTreeMap::new();
    for item in items {
        let (key, value) = item.map_err(|err| {
            // The parser's own message names the offending fragment but not the
            // fix, and the case people actually hit is `KEY=a value`: every other
            // dotenv implementation accepts it, here the value must be quoted.
            let hint = match &err {
                dotenvy::Error::LineParse(..) => {
                    " (a line must be KEY=VALUE, and a value containing spaces or '#' must be quoted: KEY=\"a value\")"
                }
                _ => "",
            };
            anyhow!(
                "cannot parse env-file {path}: {err}{hint}",
                path = path.display()
            )
        })?;
        // dotenvy guarantees a non-empty key. A NUL byte cannot be passed to a
        // process, so catch it here: the failure then names the file instead of
        // surfacing later as a spawn error.
        if value.contains('\0') {
            bail!(
                "env-file {}: the value of {key:?} contains a NUL byte",
                path.display()
            );
        }
        if vars.insert(key.clone(), value).is_some() {
            bail!(
                "env-file {}: {key:?} is declared more than once; keep a single line",
                path.display()
            );
        }
    }
    Ok(vars)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse env-file text without touching the filesystem; `path` only names the
    /// source in the error messages.
    fn parse(text: &str) -> Result<BTreeMap<String, String>> {
        collect(Path::new(".env"), dotenvy::from_read_iter(text.as_bytes()))
    }

    #[test]
    fn reads_comments_export_and_quotes() {
        let vars = parse(
            "\
# a comment\n\
\n\
PLAIN=value\n\
export EXPORTED=\"other value\"\n\
SINGLE='literal $HOME'\n\
ESCAPED=\"a\\nb\"\n\
TRAILING=end # trailing comment\n\
HASH=keep#this\n",
        )
        .unwrap();
        assert_eq!(vars["PLAIN"], "value");
        assert_eq!(vars["EXPORTED"], "other value");
        // Single quotes are literal: no substitution, no escapes.
        assert_eq!(vars["SINGLE"], "literal $HOME");
        assert_eq!(vars["ESCAPED"], "a\nb");
        assert_eq!(vars["TRAILING"], "end");
        assert_eq!(vars["HASH"], "keep#this");
    }

    #[test]
    fn substitutes_from_the_file() {
        let vars = parse(
            "\
HOST=db.internal\n\
URL=\"http://${HOST}:5432\"\n\
BRACED=${HOST}/x\n\
BARE=$HOST/y\n",
        )
        .unwrap();
        assert_eq!(vars["URL"], "http://db.internal:5432");
        assert_eq!(vars["BRACED"], "db.internal/x");
        assert_eq!(vars["BARE"], "db.internal/y");
    }

    #[test]
    fn unquoted_spaces_are_rejected_with_the_fix_in_the_message() {
        // dotenv requires quoting here; guessing would silently truncate the value.
        let err = parse("KEY=two words\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("cannot parse env-file"), "{text}");
        assert!(text.contains("two words"), "{text}");
        assert!(text.contains("must be quoted"), "{text}");

        // Quoting it is all it takes.
        assert_eq!(parse("KEY=\"two words\"\n").unwrap()["KEY"], "two words");

        // A line that is not a KEY=VALUE pair at all gets the same fix.
        let err = parse("JUST WORDS\n").unwrap_err();
        assert!(err.to_string().contains("must be KEY=VALUE"), "{err}");
    }

    #[test]
    fn duplicate_keys_are_rejected() {
        let err = parse("PORT=1\nOTHER=2\nPORT=3\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("more than once"), "{text}");
        assert!(text.contains("PORT"), "{text}");
    }

    #[test]
    fn broken_lines_are_rejected() {
        for text in ["THIS IS NOT A LINE\n", "1BAD=1\n"] {
            let err = parse(text).unwrap_err();
            assert!(err.to_string().contains("cannot parse env-file"), "{err}");
        }
    }

    #[test]
    fn nul_bytes_are_rejected() {
        let err = parse("KEY=before\0after\n").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("NUL"), "{text}");
        assert!(text.contains("KEY"), "{text}");
    }

    #[test]
    fn missing_file_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.env");
        let err = load(&path).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("cannot read env-file"), "{text}");
        assert!(text.contains(&path.display().to_string()), "{text}");
    }

    #[test]
    fn load_reads_a_real_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "# hi\nA=1\nB=\"two words\"\n").unwrap();
        let vars = load(&path).unwrap();
        assert_eq!(vars.len(), 2);
        assert_eq!(vars["A"], "1");
        assert_eq!(vars["B"], "two words");
    }

    #[test]
    fn a_directory_is_an_error_not_a_panic() {
        // Opening a directory succeeds on Linux; the failure surfaces while
        // reading, so only the path and the file kind are guaranteed.
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("env-file"), "{text}");
        assert!(text.contains(&dir.path().display().to_string()), "{text}");
    }
}
