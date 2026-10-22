//! Legacy recognition is confined to this offline crate, never the native engine.
use crate::{
    native::{Kind, clip_pages},
    seal::Export,
    storage::hash,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct Span {
    pub start: u64,
    pub end: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Repair {
    pub original: u64,
    pub importer_version: String,
    pub source_line_sha256: String,
    pub original_text_sha256: String,
    pub output_text_sha256: String,
    pub path: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub replacement: String,
    pub reason: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Plan {
    pub texts: Vec<String>,
    pub spans: Vec<Span>,
    pub repairs: Vec<Repair>,
    pub iterations: usize,
}
#[derive(Clone, Copy)]
enum Call {
    Zoom { id: u64, n: u64 },
    Date { id: u64 },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Zoom {
    id: u64,
    n: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Date {
    id: u64,
}
fn call(text: &str) -> Result<Option<Call>> {
    let first = text.split_whitespace().next().unwrap_or("");
    match first {
        "zoom" => {
            let z: Zoom = serde_json::from_str(
                text.strip_prefix("zoom ")
                    .context("malformed zoom envelope")?,
            )
            .context("malformed zoom arguments")?;
            Ok(Some(Call::Zoom { id: z.id, n: z.n }))
        }
        "date" => {
            let d: Date = serde_json::from_str(
                text.strip_prefix("date ")
                    .context("malformed date envelope")?,
            )
            .context("malformed date arguments")?;
            Ok(Some(Call::Date { id: d.id }))
        }
        _ => Ok(None),
    }
}
fn spans(export: &Export, texts: &[String]) -> Result<Vec<Span>> {
    let mut end = 0_u64;
    export
        .records
        .iter()
        .zip(texts)
        .map(|(r, t)| {
            let start = end;
            let pages = clip_pages(t).len().max(r.images.len().div_ceil(20));
            end = end
                .checked_add(pages as u64)
                .context("native page count overflow")?;
            Ok(Span { start, end })
        })
        .collect()
}
fn target(map: &[Span], id: u64, n: u64, cutoff: u64) -> Result<Span> {
    ensure!(
        n.is_power_of_two() && id.is_multiple_of(n),
        "invalid old binary span {id}+{n}"
    );
    let end = id.checked_add(n).context("old span overflow")?;
    ensure!(
        end <= cutoff && end <= map.len() as u64,
        "future/out-of-cutoff target {id}+{n}"
    );
    let s = Span {
        start: map[id as usize].start,
        end: map[end as usize - 1].end,
    };
    let len = s.end - s.start;
    ensure!(
        len.is_power_of_two() && s.start.is_multiple_of(len),
        "unrepresentable multi-span target {id}+{n} -> {}..{}; expansion policy not approved",
        s.start,
        s.end
    );
    Ok(s)
}
fn checked_zoom(map: &[Span], id: u64, n: u64, cutoff: u64) -> Result<Span> {
    let s = target(map, id, n, cutoff)?;
    if n == 1 {
        ensure!(
            s.end - s.start == 1,
            "split original requires multiple leaf calls; expansion policy not approved"
        );
    }
    if n > 1 {
        let left = target(map, id, n / 2, cutoff)?;
        let right = target(map, id + n / 2, n / 2, cutoff)?;
        ensure!(
            left.start == s.start
                && left.end == s.start + (s.end - s.start) / 2
                && right.start == left.end
                && right.end == s.end,
            "old child split differs from native parent split"
        );
    }
    Ok(s)
}
fn header(text: &str) -> bool {
    let Some((a, b)) = text.split_once('+') else {
        return false;
    };
    let Some((n, _)) = b.split_once('|') else {
        return false;
    };
    !a.is_empty()
        && a.bytes().all(|b| b.is_ascii_digit())
        && !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_digit())
}
fn transform(export: &Export, map: &[Span]) -> Result<Vec<String>> {
    let links: BTreeMap<_, _> = export
        .seal
        .result_links
        .iter()
        .map(|l| (l.result, l.call))
        .collect();
    let mut out: Vec<String> = Vec::new();
    for r in &export.records {
        let parsed = if r.kind == Kind::Tool {
            call(&r.text)?
        } else {
            None
        };
        let text = match parsed {
            Some(Call::Zoom { id, n }) => {
                let s = checked_zoom(map, id, n, r.id + 1)
                    .with_context(|| format!("zoom original {}", r.id))?;
                format!("zoom {{\"id\":{},\"n\":{}}}", s.start, s.end - s.start)
            }
            Some(Call::Date { id }) => {
                ensure!(id <= r.id && id < map.len() as u64, "future date target");
                format!("date {{\"id\":{}}}", map[id as usize].start)
            }
            None => r.text.clone(),
        };
        let text = if let Some(&c) = links.get(&r.id) {
            match call(&export.records[c as usize].text)?.context("linked call is not zoom/date")? {
                Call::Zoom { id, n: 1 } => {
                    let s = checked_zoom(map, id, 1, c + 1)?;
                    let original = &export.records[id as usize];
                    let expected = format!("{id}+0|{}: {}", original.kind.as_str(), original.text);
                    ensure!(
                        r.text == expected,
                        "leaf result {} not corroborated by authoritative original; clipped/errors/ambiguous results require explicit policy",
                        r.id
                    );
                    format!(
                        "{}+0|{}: {}",
                        s.start,
                        original.kind.as_str(),
                        out[id as usize]
                    )
                }
                Call::Zoom { id, n } => {
                    checked_zoom(map, id, n, c + 1)?;
                    let lines: Vec<_> = r.text.split('\n').collect();
                    ensure!(
                        lines.len() == 2,
                        "parent result must contain exactly two child lines"
                    );
                    let mut output = Vec::new();
                    for (j, line) in lines.iter().enumerate() {
                        let child = id + j as u64 * (n / 2);
                        let prefix = format!("{child}+{}|", n / 2);
                        let body = line
                            .strip_prefix(&prefix)
                            .context("parent result child mismatch")?;
                        let s = target(map, child, n / 2, c + 1)?;
                        output.push(format!("{}+{}|{body}", s.start, s.end - s.start));
                    }
                    output.join("\n")
                }
                Call::Date { id } => {
                    ensure!(
                        r.text == export.records[id as usize].timestamp,
                        "date result timestamp mismatch"
                    );
                    r.text.clone()
                }
            }
        } else {
            ensure!(
                r.kind != Kind::Echo || !header(&r.text),
                "unpaired address-bearing echo {}; sealed result link required",
                r.id
            );
            text
        };
        out.push(text);
    }
    Ok(out)
}
pub fn build(export: &Export) -> Result<Plan> {
    let mut texts = export
        .records
        .iter()
        .map(|r| r.text.clone())
        .collect::<Vec<_>>();
    let mut seen = BTreeSet::new();
    for iteration in 1..=32 {
        let map = spans(export, &texts)?;
        let next = transform(export, &map)?;
        if next == texts {
            let lines = export
                .original_bytes
                .strip_suffix(b"\n")
                .context("missing newline")?
                .split(|b| *b == b'\n')
                .collect::<Vec<_>>();
            let repairs = export
                .records
                .iter()
                .zip(&next)
                .filter_map(|(r, t)| {
                    if r.text == *t {
                        return None;
                    }
                    let mut prefix = r
                        .text
                        .bytes()
                        .zip(t.bytes())
                        .take_while(|(a, b)| a == b)
                        .count();
                    while !r.text.is_char_boundary(prefix) || !t.is_char_boundary(prefix) {
                        prefix -= 1;
                    }
                    let mut suffix = r.text[prefix..]
                        .bytes()
                        .rev()
                        .zip(t[prefix..].bytes().rev())
                        .take_while(|(a, b)| a == b)
                        .count();
                    while !r.text.is_char_boundary(r.text.len() - suffix)
                        || !t.is_char_boundary(t.len() - suffix)
                    {
                        suffix -= 1;
                    }
                    Some(Repair {
                        original: r.id,
                        importer_version: env!("CARGO_PKG_VERSION").into(),
                        source_line_sha256: hash(lines[r.id as usize]),
                        original_text_sha256: hash(r.text.as_bytes()),
                        output_text_sha256: hash(t.as_bytes()),
                        path: "/text".into(),
                        start_byte: prefix,
                        end_byte: r.text.len() - suffix,
                        replacement: t[prefix..t.len() - suffix].into(),
                        reason: "corroborated structured zoom/date address repair (v1)".into(),
                    })
                })
                .collect();
            return Ok(Plan {
                texts: next,
                spans: map,
                repairs,
                iterations: iteration,
            });
        }
        ensure!(
            seen.insert(hash(&serde_json::to_vec(&next)?)),
            "reference/page fixed-point cycle"
        );
        texts = next;
    }
    bail!("reference/page plan failed to converge within 32 iterations")
}
