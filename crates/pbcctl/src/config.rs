//! `pbcctl max`: edit `max_pct` in pbcd's config.toml in place, keeping its comments.

use anyhow::{bail, Context, Result};
use std::path::Path;

pub const KEY: &str = "max_pct";
/// pbcd clamps `max_pct` to this range.
pub const RANGE: std::ops::RangeInclusive<u8> = 50..=100;

/// `max_pct` as the file sets it, or None if it's unset (commented out or absent).
pub fn get(text: &str) -> Result<Option<u8>> {
    let v: toml::Table = toml::from_str(text).context("config.toml doesn't parse")?;
    match v.get(KEY) {
        None => Ok(None),
        Some(toml::Value::Integer(n)) => Ok(u8::try_from(*n).ok()),
        Some(other) => bail!("{KEY} = {other} isn't a number"),
    }
}

/// `hold_min` as the file sets it, or pbcd's default.
pub fn hold_min(text: &str) -> u8 {
    toml::from_str::<toml::Table>(text)
        .ok()
        .and_then(|t| t.get("hold_min")?.as_integer())
        .and_then(|n| u8::try_from(n).ok())
        .unwrap_or(70)
}

/// Is this line `max_pct = ...`, commented out or not? Returns whether it's commented.
fn key_line(line: &str) -> Option<bool> {
    let t = line.trim_start();
    let (commented, rest) = match t.strip_prefix('#') {
        Some(r) => (true, r.trim_start()),
        None => (false, t),
    };
    let rest = rest.strip_prefix(KEY)?;
    rest.trim_start().starts_with('=').then_some(commented)
}

/// The config text with `max_pct` set to `v`, or unset (commented back to its default) for None.
///
/// Sets the live line if there is one, else uncomments the shipped `# max_pct = 100`
/// example, else adds the key before the first table, where it's still top-level.
pub fn set(text: &str, v: Option<u8>) -> Result<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let live = lines.iter().position(|l| key_line(l) == Some(false));
    let example = lines.iter().position(|l| key_line(l) == Some(true));
    // Only the top level counts: a `max_pct` example below a table header would land in that table.
    let first_table = lines
        .iter()
        .position(|l| l.trim_start().starts_with('['))
        .unwrap_or(lines.len());
    match v {
        Some(n) => {
            let line = format!("{KEY} = {n}");
            match (live, example) {
                (Some(i), _) => lines[i] = line,
                (None, Some(i)) if i < first_table => lines[i] = line,
                _ => {
                    if first_table < lines.len() {
                        lines.insert(first_table, String::new());
                    }
                    lines.insert(first_table, line);
                }
            }
        }
        None => {
            if let Some(i) = live {
                lines[i] = format!("# {KEY} = {}", RANGE.end());
            }
        }
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') || text.is_empty() {
        out.push('\n');
    }
    // Never write a file pbcd would reject.
    let got = get(&out).context("edited config doesn't parse")?;
    if got != v {
        bail!("edited config reads {KEY} = {got:?}, expected {v:?}");
    }
    Ok(out)
}

/// Replace `path` with `text`, keeping its permissions. A rename, so pbcd never reads half a file.
pub fn write(path: &Path, text: &str) -> Result<()> {
    let perms = std::fs::metadata(path).map(|m| m.permissions()).ok();
    let tmp = path.with_extension("toml.pbcctl");
    let r = (|| {
        std::fs::write(&tmp, text)?;
        if let Some(p) = perms {
            std::fs::set_permissions(&tmp, p)?;
        }
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = r {
        let _ = std::fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            bail!(
                "writing {}: {e}\nhint: this needs root (try sudo)",
                path.display()
            );
        }
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = include_str!("../../../dist/config.toml");

    #[test]
    fn shipped_example_is_uncommented_in_place() {
        let out = set(SHIPPED, Some(92)).unwrap();
        assert_eq!(get(&out).unwrap(), Some(92));
        assert_eq!(out.lines().count(), SHIPPED.lines().count());
        assert!(out.contains("# the hold range."), "comments kept");
        // And back again.
        let back = set(&out, None).unwrap();
        assert_eq!(back, SHIPPED);
    }

    #[test]
    fn live_line_is_replaced() {
        let out = set("hold_min = 60\nmax_pct = 90\n", Some(85)).unwrap();
        assert_eq!(out, "hold_min = 60\nmax_pct = 85\n");
    }

    #[test]
    fn added_before_the_first_table() {
        let text = "mode = \"control\"\n[[schedule]]\ndays = [\"mon\"]\nfull_by = \"08:00\"\n";
        let out = set(text, Some(92)).unwrap();
        assert!(
            out.starts_with("mode = \"control\"\nmax_pct = 92\n\n[[schedule]]"),
            "{out}"
        );
        assert_eq!(get(&out).unwrap(), Some(92));
    }

    #[test]
    fn example_inside_a_table_is_not_uncommented() {
        let text = "[[schedule]]\n# max_pct = 100\ndays = [\"mon\"]\nfull_by = \"08:00\"\n";
        let out = set(text, Some(92)).unwrap();
        assert_eq!(get(&out).unwrap(), Some(92));
        assert!(out.contains("# max_pct = 100"));
    }

    #[test]
    fn empty_file_and_unset_noop() {
        assert_eq!(set("", Some(92)).unwrap(), "max_pct = 92\n");
        assert_eq!(
            set("mode = \"control\"\n", None).unwrap(),
            "mode = \"control\"\n"
        );
    }

    #[test]
    fn similar_keys_untouched() {
        let out = set("max_pct_old = 1\n", Some(92)).unwrap();
        assert!(out.contains("max_pct_old = 1"));
        assert_eq!(get(&out).unwrap(), Some(92));
    }

    #[test]
    fn hold_min_defaults_to_70() {
        assert_eq!(hold_min(SHIPPED), 70);
        assert_eq!(hold_min("hold_min = 80\n"), 80);
    }
}
