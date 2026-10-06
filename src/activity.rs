//! Compact, task-oriented presentation. Arguments and outputs stay in private traces.
use crate::provider::ToolCall;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Presentation {
    pub preview: String,
    pub input_lines: Option<u64>,
    pub output_lines: Option<u64>,
    pub exit: Option<String>,
    pub partial: bool,
}
pub fn lines(text: &str) -> u64 {
    text.lines().count() as u64
}
pub fn safe(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    static SECRETS: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r#"(?i)((?:[\w-]*(?:token|secret|password|passwd|api[_-]?key)[\w-]*|authorization)\s*(?:=|:|\s)\s*)(?:(?:bearer|basic)\s+)?(?:\"[^\"]*\"|'[^']*'|[^\s;,\)]+)"#
    ).unwrap()
    });
    static BLOBS: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?:sk-[A-Za-z0-9_-]+|[A-Za-z0-9_+/=-]{80,})").unwrap());
    static URLS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"https?://[^\s'\"<>]+"#).unwrap());
    let text = URLS.replace_all(text, |m: &regex::Captures<'_>| url_preview(&m[0]));
    let text = SECRETS.replace_all(&text, "$1[redacted]");
    let text = BLOBS.replace_all(&text, "[blob]");
    let plain = text
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('`', "ˋ")
        .replace(['\u{200b}', '\u{202e}', '\u{202d}'], "");
    if plain.encode_utf16().count() <= max {
        plain
    } else {
        format!("{}…", crate::ui::truncate(&plain, max.saturating_sub(1)))
    }
}
fn string<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
fn url_preview(raw: &str) -> String {
    reqwest::Url::parse(raw)
        .ok()
        .map(|url| format!("{}{}", url.host_str().unwrap_or(""), url.path()))
        .unwrap_or_default()
}
impl Presentation {
    pub fn call(call: &ToolCall) -> Self {
        let a = &call.arguments;
        let mut p = Self::default();
        p.preview = safe(
            &match call.name.as_str() {
                "shell" => {
                    p.input_lines = Some(lines(string(a, "command")));
                    shell_preview(string(a, "command"))
                }
                "read" | "write" => {
                    if call.name == "write" {
                        p.input_lines = Some(lines(string(a, "text")));
                    }
                    string(a, "path").into()
                }
                "browser" => format!(
                    "{} {}",
                    string(a, "action"),
                    if a["url"].is_string() {
                        url_preview(string(a, "url"))
                    } else {
                        string(a, "selector").to_owned()
                    }
                ),
                "web_search" => string(a, "query").into(),
                "web_fetch" => url_preview(string(a, "url")),
                "skill" => format!("{} {}", string(a, "action"), string(a, "file")),
                // External arguments and prompt text may contain credentials or personal data.
                "mcp" => string(a, "action").into(),
                "zoom" | "date" => format!("{}+{}", a["id"], a["n"].as_u64().unwrap_or(0)),
                "monitor" | "wakeup" | "list_agents" => string(a, "action").into(),
                _ => String::new(),
            },
            64,
        );
        p
    }
    pub fn finish(&mut self, tool: &str, output: &str) {
        if tool == "shell" {
            self.exit = output
                .strip_prefix("exit: ")
                .and_then(|s| s.lines().next())
                .map(str::to_owned);
            if let Some(n) = output.lines().find_map(|l| {
                l.strip_prefix("output_lines: ")
                    .and_then(|n| n.parse().ok())
            }) {
                self.output_lines = Some(n);
            } else if let Some((_, streams)) = output.split_once("stdout:\n") {
                let (out, err) = streams.split_once("\nstderr:\n").unwrap_or((streams, ""));
                self.output_lines = Some(lines(out) + lines(err));
                self.partial =
                    output.contains("… output omitted …") || output.contains("characters omitted");
            }
        } else if tool == "read" && self.output_lines.is_none() && !output.starts_with("Error:") {
            self.output_lines = Some(lines(output));
            self.partial =
                output.contains("… output omitted …") || output.contains("characters omitted");
        }
    }
    pub fn failed(&self) -> bool {
        self.exit.as_deref().is_some_and(|e| e != "0")
    }
    pub fn row(&self, label: &str, state: &str, millis: i64) -> String {
        let icon = match state {
            "running" => "◇",
            "background" => "↗",
            "returned" => "↙",
            "done" => "✓",
            _ => "✗",
        };
        let counts = match (self.input_lines, self.output_lines) {
            (Some(i), Some(o)) => format!(
                " · ↑ {i} ↓ {}{o} lines",
                if self.partial { "≥ " } else { "" }
            ),
            (Some(i), None) => format!(" · ↑ {i}"),
            (None, Some(o)) => format!(" · ↓ {}{o} lines", if self.partial { "≥ " } else { "" }),
            _ => String::new(),
        };
        let exit = self
            .exit
            .as_ref()
            .map(|e| format!(" · exit {}", safe(e, 8)))
            .unwrap_or_default();
        let duration = if (0..1000).contains(&millis) && !matches!(state, "running" | "background")
        {
            format!("{}ms", millis.max(0))
        } else {
            format!("{:.1}s", millis.max(0) as f64 / 1000.0)
        };
        let suffix = format!("{counts}{exit} · {duration}");
        let label = safe(
            label,
            64.min(150usize.saturating_sub(suffix.encode_utf16().count() + 3)),
        );
        let preview_budget = 150usize
            .saturating_sub(label.encode_utf16().count() + suffix.encode_utf16().count() + 6);
        let preview = safe(&self.preview, preview_budget);
        format!(
            "{icon} {label}{}{suffix}",
            if preview.is_empty() {
                String::new()
            } else {
                format!(" · {preview}")
            }
        )
    }
}

/// Select useful effects/tests instead of clipping setup, imports or large literals.
fn shell_preview(command: &str) -> String {
    shell_preview_depth(command, 0)
}
fn shell_preview_depth(command: &str, depth: u8) -> String {
    if depth >= 4 {
        return command.lines().next().unwrap_or("command").into();
    }
    let mut candidates = Vec::new();
    let mut python = false;
    let mut heredoc_end = None;
    for line in command.lines().take(4096) {
        let line = line.trim();
        if heredoc_end.as_deref() == Some(line) {
            python = false;
            heredoc_end = None;
            continue;
        }
        if !python && let Some((opener, delimiter)) = line.split_once("<<") {
            let delimiter = delimiter
                .trim()
                .trim_start_matches('-')
                .trim()
                .trim_matches(['\'', '"']);
            heredoc_end = Some(delimiter.to_owned());
            python = opener
                .split_whitespace()
                .any(|w| matches!(w, "python" | "python3"));
            if opener.contains('>') && !python {
                candidates.push((
                    8,
                    format!(
                        "write {}",
                        opener.split('>').next_back().unwrap_or("").trim()
                    ),
                ));
            }
            continue;
        }
        for part in chain(line) {
            let t = part.trim();
            if t.is_empty()
                || t.starts_with('#')
                || t.starts_with("set ")
                || t.starts_with("cd ")
                || t.starts_with("export ")
                || t.starts_with("source ")
            {
                continue;
            }
            if !python
                && t.starts_with("nix develop ")
                && let Some((_, inner)) = t.split_once("--command ")
            {
                candidates.push((9, shell_preview_depth(inner, depth + 1)));
                continue;
            }
            if !python
                && t.split_whitespace()
                    .any(|w| matches!(w, "python" | "python3"))
                && let Some((_, code)) = t.split_once("-c ")
                && let Some(quote) = code.chars().next().filter(|c| matches!(c, '\'' | '"'))
                && let Some(end) = code[1..].rfind(quote)
            {
                candidates.push((
                    8,
                    shell_preview_depth(
                        &format!(
                            "python3 - <<'PANTHEON_PREVIEW'\n{}\nPANTHEON_PREVIEW",
                            &code[1..end + 1]
                        ),
                        depth + 1,
                    ),
                ));
                continue;
            }
            if !python
                && t.split_whitespace()
                    .next()
                    .is_some_and(|w| matches!(w, "bash" | "sh"))
                && let Some((_, body)) = t.split_once("-c ")
                && let Some(quote) = body.chars().next().filter(|c| matches!(c, '\'' | '"'))
                && let Some(end) = body[1..].rfind(quote)
            {
                candidates.push((9, shell_preview_depth(&body[1..end + 1], depth + 1)));
                continue;
            }
            let score = if python {
                if t.starts_with("import ")
                    || t.starts_with("from ")
                    || t.starts_with("def ")
                    || t.starts_with("class ")
                {
                    continue;
                }
                if [
                    ".write_",
                    ".execute(",
                    ".run(",
                    ".commit(",
                    ".poll(",
                    ".click(",
                    ".submit(",
                    ".unlink(",
                ]
                .iter()
                .any(|s| t.contains(s))
                {
                    9
                } else if t.contains('(') {
                    6
                } else {
                    1
                }
            } else if [
                "cargo ", "git ", "pytest ", "nix ", "npm ", "pnpm ", "rg ", "curl ", "rm ", "mv ",
                "cp ",
            ]
            .iter()
            .any(|s| t.starts_with(s))
            {
                8
            } else if t.starts_with("echo ") || t.starts_with("printf ") {
                2
            } else {
                5
            };
            candidates.push((
                score,
                if python {
                    format!("python: {t}")
                } else {
                    t.into()
                },
            ));
        }
    }
    candidates
        .into_iter()
        .enumerate()
        .max_by_key(|(i, (score, _))| (*score, *i))
        .map(|(_, (_, text))| text)
        .unwrap_or_else(|| "command".into())
}
fn chain(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut quote = 0;
    let mut escaped = false;
    let mut start = 0;
    let mut parts = vec![];
    for (i, &b) in bytes.iter().enumerate() {
        if i < start {
            continue;
        }
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' && quote != b'\'' {
            escaped = true;
            continue;
        }
        if quote != 0 {
            if b == quote {
                quote = 0;
            }
            continue;
        }
        if b == b'\'' || b == b'"' {
            quote = b;
            continue;
        }
        if b == b';' || (b == b'&' && bytes.get(i + 1) == Some(&b'&')) {
            parts.push(&line[start..i]);
            start = i + if b == b'&' { 2 } else { 1 };
        }
    }
    parts.push(&line[start.min(line.len())..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn previews_select_effects_and_keep_quoted_chains_intact() {
        assert_eq!(
            shell_preview("nix develop --command bash -c 'set -e; cargo test --locked; echo done'"),
            "cargo test --locked"
        );
        assert_eq!(
            shell_preview("python3 -c 'import os; os.remove(\"target\")'"),
            "python: os.remove(\"target\")"
        );
        assert_eq!(
            shell_preview("set -e\ncd /tmp && cargo test --locked\necho done"),
            "cargo test --locked"
        );
        assert_eq!(
            shell_preview(
                "python3 - <<'PY'\nimport os\nfrom pathlib import Path\nx = 42\ncua_pr_watch.poll(); print(cua_pr_watch.tail(30))\nPY"
            ),
            "python: cua_pr_watch.poll()"
        );
        assert_eq!(
            chain("echo 'a;b && c'; sleep 20"),
            vec!["echo 'a;b && c'", " sleep 20"]
        );
    }
    #[test]
    fn previews_hide_credentials_and_never_break_fences_or_rows() {
        assert!(safe("a preview", 0).is_empty());
        let p = Presentation::call(&ToolCall {
            id: "c".into(),
            name: "shell".into(),
            arguments: json!({"command":"curl -H 'Authorization: Bearer private-key' https://example.com\nPASSWORD=private-password"}),
        });
        assert!(!p.preview.contains("private-key"));
        assert!(!safe("TOKEN='secret value' ```\nhello", 64).contains("secret value"));
        let mut p = p;
        p.finish(
            "shell",
            "exit: 2\noutput_lines: 31\nstdout:\nignored\nstderr:\n",
        );
        let row = p.row("shell", "returned", 6700);
        assert!(row.starts_with("↙ shell"));
        assert!(row.contains("↓ 31 lines"));
        assert!(row.contains("exit 2"));
        assert!(row.encode_utf16().count() <= 150);
        assert!(!row.contains('\n'));
        assert!(!row.contains("```"));
        p.input_lines = Some(u64::MAX);
        p.output_lines = Some(u64::MAX);
        assert!(
            p.row(&"long".repeat(30), "returned", i64::MAX)
                .encode_utf16()
                .count()
                <= 150
        );
    }
}
