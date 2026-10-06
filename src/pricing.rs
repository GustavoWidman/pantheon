//! Public list prices, independently refreshed from authenticated model discovery.
//! No catalog IDs, prices, account discounts or subscription rates are guessed.
use crate::provider::Provider;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::RwLock};

pub(crate) const OPENAI: &str = "https://developers.openai.com/api/docs/pricing";
pub(crate) const ANTHROPIC: &str = "https://platform.claude.com/docs/en/about-claude/pricing";
const REFRESH_SECONDS: i64 = 6 * 3600;
const STALE_SECONDS: i64 = 24 * 3600;
const RETRY_SECONDS: i64 = 300;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Rates {
    context: String,
    input_tokens_max_inclusive: Option<u64>,
    input_tokens_min_exclusive: Option<u64>,
    input: f64,
    output: f64,
    cached_input: Option<f64>,
    cache_write: Option<f64>,
    cache_write_1h: Option<f64>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Snapshot {
    source: String,
    observed_at: i64,
    models: BTreeMap<String, Vec<Rates>>,
}
#[derive(Default)]
pub(crate) struct Prices {
    snapshots: RwLock<BTreeMap<String, Snapshot>>,
    attempts: RwLock<BTreeMap<String, i64>>,
}
impl Prices {
    pub fn initialize(&self, dir: &Path) {
        use std::io::Read;
        let snapshots = std::fs::File::open(dir.join("model-pricing.json"))
            .ok()
            .and_then(|f| {
                serde_json::from_reader::<_, BTreeMap<String, Snapshot>>(f.take(1_048_576)).ok()
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|(provider, snapshot)| {
                source(provider) == Some(snapshot.source.as_str())
                    && !snapshot.models.is_empty()
                    && snapshot.models.len() <= 1000
                    && snapshot.models.iter().all(|(id, rates)| {
                        valid_id(id) && !rates.is_empty() && rates.iter().all(valid_rates)
                    })
            })
            .collect();
        *self.snapshots.write().unwrap() = snapshots;
    }
    pub async fn refresh(&self, dir: &Path, providers: &[&str], api: &Provider) {
        let needed = providers
            .iter()
            .map(|p| if *p == "codex" { "openai" } else { *p })
            .collect::<std::collections::BTreeSet<_>>();
        for provider in needed {
            let Some(url) = source(provider) else {
                continue;
            };
            let now = crate::store::now();
            let fresh = self
                .snapshots
                .read()
                .unwrap()
                .get(provider)
                .is_some_and(|s| {
                    (0..REFRESH_SECONDS).contains(&(now.saturating_sub(s.observed_at)))
                });
            if fresh {
                continue;
            }
            {
                let mut attempts = self.attempts.write().unwrap();
                if attempts
                    .get(provider)
                    .is_some_and(|last| (0..RETRY_SECONDS).contains(&(now.saturating_sub(*last))))
                {
                    continue;
                }
                attempts.insert(provider.into(), now);
            }
            let result = async {
                let markdown = api.pricing_document(url).await?;
                parse(provider, &markdown, now)
            }
            .await;
            match result {
                Ok(snapshot) => {
                    self.snapshots
                        .write()
                        .unwrap()
                        .insert(provider.into(), snapshot);
                    if let Err(error) = self.persist(dir) {
                        tracing::warn!(error=%error,"pricing snapshot could not be saved");
                    }
                }
                Err(error) => {
                    tracing::warn!(provider,error=%error,"pricing discovery failed; retaining dated snapshot");
                }
            }
        }
    }
    fn persist(&self, dir: &Path) -> Result<()> {
        use std::io::Write;
        let bytes = serde_json::to_vec(&*self.snapshots.read().unwrap())?;
        let temp = dir.join(format!(".model-pricing-{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, dir.join("model-pricing.json"))?;
            std::fs::File::open(dir)?.sync_all()?;
            Ok(())
        })();
        let _ = std::fs::remove_file(temp);
        result
    }
    pub fn report(&self, provider: &str, id: &str, name: &str, now: i64) -> Value {
        let api_provider = if provider == "codex" {
            "openai"
        } else {
            provider
        };
        let snapshots = self.snapshots.read().unwrap();
        let Some(snapshot) = snapshots.get(api_provider) else {
            return Value::Null;
        };
        // Exact public IDs first. Claude's public price table uses display names;
        // only an exact normalized display-name match is an allowed fallback.
        let display_id = claude_id(name);
        let matched = if snapshot.models.contains_key(id) {
            Some((id, "exact_id"))
        } else if api_provider == "anthropic" {
            display_id
                .as_deref()
                .filter(|id| snapshot.models.contains_key(*id))
                .map(|id| (id, "catalog_display_name"))
        } else {
            None
        };
        let Some((matched_id, matched_by)) = matched else {
            return Value::Null;
        };
        let age = now.saturating_sub(snapshot.observed_at);
        json!({
            "currency":"USD","unit":"per_1m_tokens","service_tier":"standard",
            "applicability":if provider=="codex"{"api_reference_only"}else{"api_list_price"},
            "api_provider":api_provider,"matched_model":matched_id,"matched_by":matched_by,
            "source":snapshot.source,"observed_at":snapshot.observed_at,
            "age_seconds":age.max(0),"stale":!(0..STALE_SECONDS).contains(&age),
            "rates":snapshot.models[matched_id],
            "scope":"Public Standard text-token list prices, not an account bill. Excludes service-tier/geography premiums, tool fees, taxes and negotiated discounts. Context labels without numeric bounds must be checked in the source. Subscription quota cannot be inferred from API prices."
        })
    }
}
fn source(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some(OPENAI),
        "anthropic" => Some(ANTHROPIC),
        _ => None,
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b".-".contains(&c))
}
fn valid_rates(r: &Rates) -> bool {
    [
        Some(r.input),
        Some(r.output),
        r.cached_input,
        r.cache_write,
        r.cache_write_1h,
    ]
    .into_iter()
    .flatten()
    .all(|n| n.is_finite() && n >= 0.)
}
fn cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}
fn dollars(cell: &str) -> Result<Option<f64>> {
    if matches!(cell, "-" | "—" | "N/A") {
        return Ok(None);
    }
    let text = cell
        .strip_prefix('$')
        .context("Price cell has no dollar unit")?;
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let number: f64 = text[..end].parse().context("Invalid numeric price")?;
    let suffix = text[end..].trim();
    ensure!(
        suffix.is_empty() || suffix.starts_with("/ MTok") || suffix.starts_with("<sup>"),
        "Unrecognized price unit"
    );
    ensure!(number.is_finite() && number >= 0., "Invalid price range");
    Ok(Some(number))
}
fn claude_id(name: &str) -> Option<String> {
    let name = name.split(" (").next()?.trim();
    if !name.starts_with("Claude ") || name.contains(['[', '/', '<', '`']) {
        return None;
    }
    let id = name.to_lowercase().replace([' ', '.'], "-");
    valid_id(&id).then_some(id)
}
fn parse(provider: &str, markdown: &str, observed_at: i64) -> Result<Snapshot> {
    let (section, headers): (&str, Vec<&str>) = match provider {
        "openai" => (
            "### Standard pricing data",
            vec![
                "Model",
                "Short context input",
                "Short context cached input",
                "Short context cache writes",
                "Short context output",
                "Long context input",
                "Long context cached input",
                "Long context cache writes",
                "Long context output",
            ],
        ),
        "anthropic" => (
            "## Model pricing",
            vec![
                "Model",
                "Base input tokens",
                "5m cache writes",
                "1h cache writes",
                "Cache hits and refreshes",
                "Output tokens",
            ],
        ),
        _ => anyhow::bail!("Unsupported pricing provider"),
    };
    let lines = markdown.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|l| l.trim() == section)
        .context("Official pricing section is missing")?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find(|(_, line)| line.trim().starts_with('#'))
        .map(|(i, _)| i)
        .unwrap_or(lines.len());
    let table = &lines[start + 1..end];
    let header = table
        .iter()
        .position(|line| cells(line) == headers)
        .context("Official pricing table schema changed")?;
    let separator = cells(table.get(header + 1).context("Missing table separator")?);
    ensure!(
        separator.len() == headers.len()
            && separator
                .iter()
                .all(|s| !s.is_empty() && s.contains('-') && s.bytes().all(|c| b":-".contains(&c))),
        "Invalid pricing table separator"
    );
    let boundary = if provider == "openai" {
        regex::Regex::new(r"Short context: (?:≤|<=)([0-9]+)([KM]?) input tokens\. Long context: >")
            .unwrap()
            .captures(markdown)
            .and_then(|c| {
                c[1].parse::<u64>().ok()?.checked_mul(match &c[2] {
                    "K" => 1000,
                    "M" => 1_000_000,
                    _ => 1,
                })
            })
    } else {
        None
    };
    let mut models = BTreeMap::new();
    let mut rows = table[header + 2..].iter();
    for line in rows
        .by_ref()
        .take_while(|line| line.trim().starts_with('|'))
    {
        let row = cells(line);
        ensure!(
            row.len() == headers.len(),
            "Official pricing row schema changed"
        );
        let id = if provider == "openai" {
            row[0]
                .split(" (")
                .next()
                .unwrap()
                .trim_matches('`')
                .to_owned()
        } else {
            claude_id(row[0]).context("Unrecognized Claude model label")?
        };
        ensure!(valid_id(&id), "Invalid model label in pricing table");
        let rates = if provider == "openai" {
            let mut rates = vec![];
            for (context, offset) in [("short", 1), ("long", 5)] {
                let fields = row[offset..offset + 4]
                    .iter()
                    .map(|cell| dollars(cell))
                    .collect::<Result<Vec<_>>>()?;
                if fields.iter().all(Option::is_none) {
                    continue;
                }
                rates.push(Rates {
                    context: context.into(),
                    input_tokens_max_inclusive: if context == "short" { boundary } else { None },
                    input_tokens_min_exclusive: if context == "long" { boundary } else { None },
                    input: fields[0].context("Missing input price")?,
                    cached_input: fields[1],
                    cache_write: fields[2],
                    cache_write_1h: None,
                    output: fields[3].context("Missing output price")?,
                });
            }
            rates
        } else {
            vec![Rates {
                context: "base".into(),
                input_tokens_max_inclusive: None,
                input_tokens_min_exclusive: None,
                input: dollars(row[1])?.context("Missing input price")?,
                cache_write: dollars(row[2])?,
                cache_write_1h: dollars(row[3])?,
                cached_input: dollars(row[4])?,
                output: dollars(row[5])?.context("Missing output price")?,
            }]
        };
        ensure!(
            !rates.is_empty() && rates.iter().all(valid_rates),
            "Missing valid token rates"
        );
        ensure!(
            models.insert(id, rates).is_none(),
            "Ambiguous duplicate pricing rows"
        );
        ensure!(models.len() <= 1000, "Pricing table exceeded row limit");
    }
    ensure!(!models.is_empty(), "Official price table is empty");
    Ok(Snapshot {
        source: source(provider).unwrap().into(),
        observed_at,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const OPENAI_DOC: &str = "# Pricing\n### Standard pricing data\n\n| Model | Short context input | Short context cached input | Short context cache writes | Short context output | Long context input | Long context cached input | Long context cache writes | Long context output |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n| gpt-example | $2 | $0.20 | $2.50 | $10 | $4 | $0.40 | $5 | $15 |\n| gpt-small | $0 | - | - | $1 | - | - | - | - |\n\n### Batch pricing data\n| Model | Input | Output |\n| --- | --- | --- |\n| gpt-example | $1 | $5 |\n\nShort context: ≤272K input tokens. Long context: >272K input tokens.\n";
    const CLAUDE_DOC: &str = "## Model pricing\n\n| Model | Base input tokens | 5m cache writes | 1h cache writes | Cache hits and refreshes | Output tokens |\n| :--- | --- | --- | --- | --- | --- |\n| Claude Example 4.6 | $3 / MTok | $3.75 / MTok | $6 / MTok | $0.30 / MTok<sup>1</sup> | $15 / MTok |\n\n## Cloud platform pricing\n| Model | Input | Output |\n| --- | --- | --- |\n| Claude Example 4.6 | $99 | $999 |\n";

    #[test]
    fn selects_standard_rates_and_preserves_context_and_cache_units() {
        let prices = Prices::default();
        prices
            .snapshots
            .write()
            .unwrap()
            .insert("openai".into(), parse("openai", OPENAI_DOC, 100).unwrap());
        let report = prices.report("openai", "gpt-example", "whatever", 110);
        assert_eq!(report["rates"][0]["input"], 2.0);
        assert_eq!(report["rates"][0]["cache_write"], 2.5);
        assert_eq!(report["rates"][1]["output"], 15.0);
        assert_eq!(report["rates"][0]["input_tokens_max_inclusive"], 272000);
        assert_eq!(report["rates"][1]["input_tokens_min_exclusive"], 272000);
        assert_eq!(report["unit"], "per_1m_tokens");
        let small = prices.report("openai", "gpt-small", "", 110);
        assert_eq!(small["rates"].as_array().unwrap().len(), 1);
        assert_eq!(small["rates"][0]["input"], 0.0);
        assert!(small["rates"][0]["cached_input"].is_null());
        assert!(
            prices
                .report("openai", "gpt-example-20261006", "gpt-example", 110)
                .is_null()
        );
        assert_eq!(
            prices.report("codex", "gpt-example", "", 110)["applicability"],
            "api_reference_only"
        );
    }

    #[test]
    fn claude_matches_provider_display_name_and_reads_cache_write_durations() {
        let prices = Prices::default();
        prices.snapshots.write().unwrap().insert(
            "anthropic".into(),
            parse("anthropic", CLAUDE_DOC, 100).unwrap(),
        );
        let report = prices.report(
            "anthropic",
            "claude-example-4-6-20260101",
            "Claude Example 4.6",
            110,
        );
        assert_eq!(report["matched_model"], "claude-example-4-6");
        assert_eq!(report["matched_by"], "catalog_display_name");
        assert_eq!(report["rates"][0]["cache_write"], 3.75);
        assert_eq!(report["rates"][0]["cache_write_1h"], 6.0);
        assert_eq!(report["rates"][0]["cached_input"], 0.30);
        assert_eq!(
            prices.report("anthropic", "claude-example-4-6", "", 110)["matched_by"],
            "exact_id"
        );
        assert!(
            prices
                .report("anthropic", "unknown", "Claude Example 4.5", 110)
                .is_null()
        );
    }

    #[test]
    fn malformed_tables_do_not_turn_unknown_or_wrong_units_into_prices() {
        for text in [
            OPENAI_DOC.replace("### Standard pricing data", "### Fast pricing data"),
            OPENAI_DOC.replace("Short context input", "Training"),
            OPENAI_DOC.replace("$2 |", "$-2 |"),
            OPENAI_DOC.replace("$2 |", "$NaN |"),
            OPENAI_DOC.replace("$2 |", "$2 / 1k tokens |"),
            OPENAI_DOC.replace("$2 |", "- |"),
            OPENAI_DOC.replace("gpt-small", "gpt-example"),
        ] {
            assert!(parse("openai", &text, 100).is_err());
        }
        let without_bounds =
            parse("openai", &OPENAI_DOC.replace("272K", "undocumented"), 100).unwrap();
        assert!(
            without_bounds.models["gpt-example"][0]
                .input_tokens_max_inclusive
                .is_none()
        );
    }

    #[tokio::test]
    async fn durable_snapshot_survives_failures_and_stale_prices_are_visible() {
        use axum::{
            Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            routing::get,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/price",
                get(
                    |State(hits): State<Arc<AtomicUsize>>, headers: HeaderMap| async move {
                        assert!(headers.get("authorization").is_none());
                        assert!(headers.get("x-api-key").is_none());
                        assert!(headers.get("chatgpt-account-id").is_none());
                        if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                            (StatusCode::OK, OPENAI_DOC)
                        } else {
                            (
                                StatusCode::OK,
                                "schema changed; private body must not appear in errors",
                            )
                        }
                    },
                ),
            )
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let provider = Provider::mock(format!("http://{address}/price"));
        let dir = tempfile::tempdir().unwrap();
        let prices = Prices::default();
        prices
            .refresh(dir.path(), &["codex", "openai"], &provider)
            .await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        prices.refresh(dir.path(), &["codex"], &provider).await;
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let restored = Prices::default();
        restored.initialize(dir.path());
        let now = crate::store::now();
        let mut config = crate::config::Config {
            state_dir: dir.path().into(),
            ..Default::default()
        };
        config.auth.codex_home = Some(dir.path().into());
        let catalog = crate::models::Catalog::default();
        catalog.initialize(&config);
        catalog.replace(
            "codex",
            vec![
                crate::models::Model::plain("gpt-example"),
                crate::models::Model::plain("unknown"),
            ],
        );
        catalog.replace("openai", vec![crate::models::Model::plain("gpt-example")]);
        let codex = catalog.report(Some("codex"), "gpt-example", 0, 25);
        assert!(codex["models"][0]["pricing"].is_null());
        assert_eq!(
            codex["models"][0]["api_price_reference"]["applicability"],
            "api_reference_only"
        );
        let openai = catalog.report(Some("openai"), "", 0, 25);
        assert_eq!(
            openai["models"][0]["pricing"]["applicability"],
            "api_list_price"
        );
        assert!(openai["models"][0]["api_price_reference"].is_null());
        assert_eq!(
            catalog.report(Some("codex"), "", 0, 25)["total"],
            2,
            "prices cannot add selectable models"
        );
        assert_eq!(
            restored.report("openai", "gpt-example", "", now)["rates"][0]["input"],
            2.0
        );
        restored
            .snapshots
            .write()
            .unwrap()
            .get_mut("openai")
            .unwrap()
            .observed_at = now - STALE_SECONDS - 1;
        restored.persist(dir.path()).unwrap();
        let bytes = std::fs::read(dir.path().join("model-pricing.json")).unwrap();
        restored.refresh(dir.path(), &["codex"], &provider).await;
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        restored.refresh(dir.path(), &["codex"], &provider).await;
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "failure retries must be bounded"
        );
        assert_eq!(
            std::fs::read(dir.path().join("model-pricing.json")).unwrap(),
            bytes
        );
        let report = restored.report("openai", "gpt-example", "", now);
        assert_eq!(report["stale"], true);
        assert_eq!(report["observed_at"], now - STALE_SECONDS - 1);
        assert_eq!(
            restored.report("openai", "gpt-example", "", now - STALE_SECONDS - 2)["stale"],
            true
        );
        server.abort();
    }

    #[tokio::test]
    #[ignore = "fetches current public provider documentation; no authentication or inference"]
    async fn official_pricing_documents_live() {
        let api = Provider::new(30).unwrap();
        for provider in ["openai", "anthropic"] {
            let doc = api
                .pricing_document(source(provider).unwrap())
                .await
                .unwrap();
            let snapshot = parse(provider, &doc, crate::store::now()).unwrap();
            assert!(!snapshot.models.is_empty());
            assert!(snapshot.models.values().flatten().all(valid_rates));
            eprintln!(
                "{provider}: parsed {} official Standard model prices",
                snapshot.models.len()
            );
        }
    }
}
