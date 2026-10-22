use crate::{native::Kind, storage::hash};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const FORMAT: &str = "pantheon-original-export-v1";
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Original {
    #[serde(rename = "i")]
    pub id: u64,
    pub kind: Kind,
    pub text: String,
    #[serde(rename = "date")]
    pub timestamp: String,
    pub size: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(default)]
    pub images: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub call: u64,
    pub result: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Seal {
    pub format: String,
    pub source_version: String,
    pub checkpoint: String,
    pub channel: String,
    pub count: u64,
    pub bytes: u64,
    pub sha256: String,
    // Explicit sealed pairing assertions; output shape/source corroboration is
    // still required. Adjacency is never used as evidence of association.
    pub result_links: Vec<Link>,
}
#[derive(Clone)]
pub struct Export {
    pub seal: Seal,
    pub manifest_bytes: Vec<u8>,
    pub original_bytes: Vec<u8>,
    pub records: Vec<Original>,
}
impl Export {
    pub fn load(path: &Path, max_bytes: u64, max_records: usize) -> Result<Self> {
        crate::storage::plain_tree(path)?;
        let mut names = fs::read_dir(path)?
            .map(|e| Ok(e?.file_name()))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        ensure!(
            names == ["originals.jsonl", "seal.json"],
            "export must contain only seal.json and originals.jsonl"
        );
        let manifest_path = path.join("seal.json");
        ensure!(
            fs::metadata(&manifest_path)?.len() <= 1_000_000,
            "seal too large"
        );
        let manifest_bytes = fs::read(manifest_path)?;
        let seal: Seal = serde_json::from_slice(&manifest_bytes).context("invalid seal")?;
        ensure!(
            seal.format == FORMAT
                && matches!(
                    seal.source_version.as_str(),
                    "pantheon-0.20.4-originals" | "pantheon-original-fixture-v1"
                ),
            "unsupported export schema"
        );
        ensure!(
            !seal.channel.is_empty() && !seal.checkpoint.is_empty(),
            "missing seal scope"
        );
        ensure!(
            seal.count > 0 && seal.count <= max_records as u64,
            "record budget exceeded or empty export"
        );
        ensure!(
            seal.bytes <= max_bytes
                && fs::metadata(path.join("originals.jsonl"))?.len() == seal.bytes,
            "byte budget/count mismatch"
        );
        let original_bytes = fs::read(path.join("originals.jsonl"))?;
        ensure!(
            hash(&original_bytes) == seal.sha256,
            "originals hash mismatch"
        );
        ensure!(
            original_bytes.ends_with(b"\n"),
            "unterminated source journal"
        );
        let mut records = Vec::new();
        for (i, line) in original_bytes
            .strip_suffix(b"\n")
            .unwrap()
            .split(|b| *b == b'\n')
            .enumerate()
        {
            let r: Original =
                serde_json::from_slice(line).with_context(|| format!("original {i}"))?;
            ensure!(
                r.id == i as u64,
                "out of order/duplicate/gapped original {i}"
            );
            chrono::DateTime::parse_from_rfc3339(&r.timestamp)
                .context("invalid original timestamp")?;
            ensure!(
                r.size == r.kind.as_str().len() + 2 + r.text.len(),
                "original size mismatch"
            );
            ensure!(
                seal.source_version != "pantheon-0.20.4-originals" || r.images.is_empty(),
                "v0.20.4 has no image-reference field; use explicit fixture schema for synthetic image cases"
            );
            ensure!(r.images.len() <= 10000, "image reference limit");
            records.push(r);
        }
        ensure!(
            records.len() as u64 == seal.count,
            "original count mismatch"
        );
        let mut results = std::collections::BTreeSet::new();
        let mut calls = std::collections::BTreeSet::new();
        for link in &seal.result_links {
            ensure!(
                link.call < link.result && link.result < seal.count,
                "invalid pairing range"
            );
            ensure!(
                results.insert(link.result) && calls.insert(link.call),
                "ambiguous pairing"
            );
            ensure!(
                records[link.call as usize].kind == Kind::Tool
                    && records[link.result as usize].kind == Kind::Echo,
                "pair kinds invalid"
            );
        }
        Ok(Self {
            seal,
            manifest_bytes,
            original_bytes,
            records,
        })
    }
    pub fn identity(&self) -> String {
        hash(&self.manifest_bytes)
    }
}
