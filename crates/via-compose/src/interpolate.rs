//! Compose variable interpolation.
//!
//! Interpolation happens once, at build time, on the machine that has the
//! `.env` file and the developer's environment. The normalized manifest that
//! ships in a bundle contains no `${...}` at all, because the device has
//! neither the developer's environment nor any business guessing at it
//! (spec section 16).
//!
//! Runtime configuration that legitimately varies per device travels as Via
//! variables and secrets instead (spec sections 34, 35).

use std::collections::BTreeMap;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum InterpolationError {
    #[error("variable {name:?} is not set{}", explain(.hint))]
    Missing { name: String, hint: Option<String> },
    #[error("unterminated variable expression starting at byte {offset}: {snippet:?}")]
    Unterminated { offset: usize, snippet: String },
}

fn explain(hint: &Option<String>) -> String {
    match hint {
        Some(h) => format!(": {h}"),
        None => String::new(),
    }
}

/// Values available for interpolation, typically process environment merged
/// with a `.env` file.
#[derive(Debug, Default, Clone)]
pub struct Vars(BTreeMap<String, String>);

impl Vars {
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture the current process environment.
    pub fn from_env() -> Self {
        Self(std::env::vars().collect())
    }

    /// Overlay values parsed from a `.env`-style file body.
    ///
    /// Existing entries win, matching Compose: the real environment takes
    /// precedence over the `.env` file.
    pub fn with_dotenv(mut self, body: &str) -> Self {
        for (k, v) in parse_dotenv(body) {
            self.0.entry(k).or_insert(v);
        }
        self
    }

    pub fn set(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.0.insert(k.into(), v.into());
        self
    }

    fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(|s| s.as_str())
    }
}

/// Parse a `.env` file body into key/value pairs.
///
/// Supports `KEY=value`, `export KEY=value`, `#` comments, and single or
/// double quoted values. Deliberately does not attempt shell semantics.
pub fn parse_dotenv(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim();
        if k.is_empty() {
            continue;
        }
        let v = v.trim();
        let v = if v.len() >= 2 && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\''))) {
            &v[1..v.len() - 1]
        } else {
            // An unquoted value ends at an inline comment.
            v.split_once(" #").map(|(a, _)| a.trim()).unwrap_or(v)
        };
        out.push((k.to_string(), v.to_string()));
    }
    out
}

/// Expand `$VAR`, `${VAR}`, `${VAR:-default}`, `${VAR-default}`,
/// `${VAR:?message}` and `${VAR?message}`. `$$` is a literal `$`.
///
/// The `:` variants treat an empty value as unset, matching Compose.
pub fn interpolate(input: &str, vars: &Vars) -> Result<String, InterpolationError> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] != b'$' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' {
                i += 1;
            }
            out.push_str(&input[start..i]);
            continue;
        }
        // bytes[i] == b'$'
        if i + 1 >= bytes.len() {
            out.push('$');
            break;
        }
        match bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'{' => {
                let close = find_closing_brace(bytes, i + 1)
                    .ok_or_else(|| InterpolationError::Unterminated {
                        offset: i,
                        snippet: input[i..(i + 24).min(input.len())].to_string(),
                    })?;
                let expr = &input[i + 2..close];
                out.push_str(&eval_expr(expr, vars)?);
                i = close + 1;
            }
            c if c == b'_' || c.is_ascii_alphabetic() => {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len() && (bytes[end] == b'_' || bytes[end].is_ascii_alphanumeric()) {
                    end += 1;
                }
                let name = &input[start..end];
                out.push_str(vars.get(name).unwrap_or(""));
                i = end;
            }
            _ => {
                // A `$` that starts nothing recognizable is a literal `$`.
                out.push('$');
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Find the `}` matching the `{` at `open`, tolerating nested braces so that
/// a default value may itself contain `${...}`.
fn find_closing_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn eval_expr(expr: &str, vars: &Vars) -> Result<String, InterpolationError> {
    for (sep, empty_counts_as_unset, is_error) in
        [(":-", true, false), ("-", false, false), (":?", true, true), ("?", false, true)]
    {
        if let Some((name, rest)) = split_once_operator(expr, sep) {
            let value = vars.get(name);
            let unset = match value {
                None => true,
                Some(v) => empty_counts_as_unset && v.is_empty(),
            };
            if !unset {
                return Ok(value.unwrap_or("").to_string());
            }
            if is_error {
                let hint = (!rest.is_empty()).then(|| interpolate(rest, vars).unwrap_or_else(|_| rest.to_string()));
                return Err(InterpolationError::Missing { name: name.to_string(), hint });
            }
            // Defaults may themselves reference variables.
            return interpolate(rest, vars);
        }
    }
    Ok(vars.get(expr).unwrap_or("").to_string())
}

/// Split on the first occurrence of `op`, ignoring one inside a nested `${}`.
fn split_once_operator<'a>(expr: &'a str, op: &str) -> Option<(&'a str, &'a str)> {
    let bytes = expr.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && expr[i..].starts_with(op) => {
                return Some((&expr[..i], &expr[i + op.len()..]));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        Vars::new().set("TAG", "1.8.3").set("EMPTY", "")
    }

    #[test]
    fn expands_both_syntaxes() {
        assert_eq!(interpolate("app:$TAG", &vars()).unwrap(), "app:1.8.3");
        assert_eq!(interpolate("app:${TAG}", &vars()).unwrap(), "app:1.8.3");
        assert_eq!(interpolate("${TAG}-${TAG}", &vars()).unwrap(), "1.8.3-1.8.3");
    }

    #[test]
    fn double_dollar_is_a_literal() {
        // Matters for Compose files that pass shell snippets through to a
        // healthcheck or command.
        assert_eq!(interpolate("$$TAG", &vars()).unwrap(), "$TAG");
        assert_eq!(interpolate("cost is $$5", &vars()).unwrap(), "cost is $5");
    }

    #[test]
    fn colon_dash_treats_empty_as_unset_but_dash_does_not() {
        assert_eq!(interpolate("${EMPTY:-fallback}", &vars()).unwrap(), "fallback");
        assert_eq!(interpolate("${EMPTY-fallback}", &vars()).unwrap(), "");
        assert_eq!(interpolate("${NOPE-fallback}", &vars()).unwrap(), "fallback");
    }

    #[test]
    fn unset_without_default_is_empty_not_an_error() {
        assert_eq!(interpolate("x${NOPE}y", &vars()).unwrap(), "xy");
    }

    #[test]
    fn required_variable_reports_its_name_and_hint() {
        let err = interpolate("${NOPE:?set this in .env}", &vars()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("NOPE"), "{msg}");
        assert!(msg.contains("set this in .env"), "{msg}");
    }

    #[test]
    fn defaults_may_nest() {
        let v = Vars::new().set("INNER", "deep");
        assert_eq!(interpolate("${OUTER:-${INNER}}", &v).unwrap(), "deep");
    }

    #[test]
    fn unterminated_expression_is_an_error_not_a_silent_truncation() {
        assert!(interpolate("${TAG", &vars()).is_err());
    }

    #[test]
    fn dotenv_handles_quotes_comments_and_export() {
        let parsed = parse_dotenv(
            r#"
# a comment
export TAG=1.8.3
QUOTED="hello world"
SINGLE='x y'
TRAILING=value # inline comment
"#,
        );
        let map: BTreeMap<_, _> = parsed.into_iter().collect();
        assert_eq!(map["TAG"], "1.8.3");
        assert_eq!(map["QUOTED"], "hello world");
        assert_eq!(map["SINGLE"], "x y");
        assert_eq!(map["TRAILING"], "value");
    }

    #[test]
    fn real_environment_wins_over_dotenv() {
        let v = Vars::new().set("TAG", "from-env").with_dotenv("TAG=from-file");
        assert_eq!(interpolate("${TAG}", &v).unwrap(), "from-env");
    }
}
