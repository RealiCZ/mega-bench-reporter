//! Lark card rendering (Task 1.6). The card layouts (regression alert /
//! trend digest) live as JSON template files in `templates/` —
//! changing a card's look
//! is a template edit + test, never string-building in code and never a BB9
//! change.
//!
//! Rendering rules (the whole template language):
//!
//! 1. `{{key}}` inside any string value is replaced with the param's value
//!    (substitution happens on decoded strings, so no JSON-escaping pitfalls).
//! 2. An element `{"tag": "__images__", "group": "<name>"}` is expanded into
//!    one `img` element per image registered under `<name>`, in order.
//! 3. Every `img` element's `img_key` is the placeholder `${image:<basename>}`
//!    and the image's path is listed in [`RenderedCard::attachments`]. The
//!    relaying agent (BB9) uploads each attachment to Lark and string-replaces
//!    the placeholder with the returned `image_key` before posting.

use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const TEMPLATE_REGRESSION_ALERT: &str = include_str!("../templates/regression_alert.json");
const TEMPLATE_TREND_DIGEST: &str = include_str!("../templates/trend_digest.json");

/// Which of the card layouts a rendered card came from — serialized alongside
/// the card so the relaying agent can log/route without inspecting the JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    RegressionAlert,
    Recovery,
    TrendDigest,
}

/// A ready-to-post card: the Lark card JSON plus the local files BB9 must
/// upload/attach when relaying it.
#[derive(Debug, Clone, Serialize)]
pub struct RenderedCard {
    pub kind: CardKind,
    pub card: Value,
    pub attachments: Vec<PathBuf>,
}

/// An image slot: local file + alt text.
#[derive(Debug, Clone)]
pub struct ImageRef {
    pub path: PathBuf,
    pub alt: String,
}

impl ImageRef {
    pub fn new(path: impl Into<PathBuf>, alt: impl Into<String>) -> Self {
        Self { path: path.into(), alt: alt.into() }
    }
}

pub fn image_placeholder(path: &Path) -> String {
    let basename = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    format!("${{image:{basename}}}")
}

/// Single-pass `{{key}}` expansion: substituted values are never re-scanned,
/// so a value that happens to contain `{{...}}` (e.g. a benchmark named that
/// way) is emitted literally instead of being expanded again. Unknown keys are
/// left as-is.
fn expand_placeholders(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) if vars.contains_key(&after[..end]) => {
                out.push_str(&vars[&after[..end]]);
                rest = &after[end + 2..];
            }
            _ => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn substitute_strings(value: &mut Value, vars: &BTreeMap<&str, String>) {
    match value {
        Value::String(s) if s.contains("{{") => *s = expand_placeholders(s, vars),
        Value::Array(items) => items.iter_mut().for_each(|v| substitute_strings(v, vars)),
        Value::Object(map) => map.values_mut().for_each(|v| substitute_strings(v, vars)),
        _ => {}
    }
}

fn img_element(image: &ImageRef) -> Value {
    serde_json::json!({
        "tag": "img",
        "img_key": image_placeholder(&image.path),
        "alt": { "tag": "plain_text", "content": image.alt },
    })
}

fn expand_image_markers(value: &mut Value, images: &BTreeMap<&str, Vec<ImageRef>>) {
    match value {
        Value::Array(items) => {
            let mut expanded = Vec::with_capacity(items.len());
            for mut item in items.drain(..) {
                let marker_group = item
                    .get("tag")
                    .and_then(Value::as_str)
                    .filter(|t| *t == "__images__")
                    .and_then(|_| item.get("group").and_then(Value::as_str))
                    .map(str::to_string);
                match marker_group {
                    Some(group) => {
                        for image in images.get(group.as_str()).into_iter().flatten() {
                            expanded.push(img_element(image));
                        }
                    }
                    None => {
                        expand_image_markers(&mut item, images);
                        expanded.push(item);
                    }
                }
            }
            *items = expanded;
        }
        Value::Object(map) => map.values_mut().for_each(|v| expand_image_markers(v, images)),
        _ => {}
    }
}

/// Renders a template: `{{key}}` substitution + `__images__` expansion.
fn render_template(
    template: &str,
    kind: CardKind,
    vars: &BTreeMap<&str, String>,
    images: &BTreeMap<&str, Vec<ImageRef>>,
    extra_attachments: &[PathBuf],
) -> anyhow::Result<RenderedCard> {
    let mut card: Value = serde_json::from_str(template)
        .map_err(|e| anyhow::anyhow!("card template for {kind:?} is not valid JSON: {e}"))?;
    substitute_strings(&mut card, vars);
    expand_image_markers(&mut card, images);
    let mut attachments: Vec<PathBuf> = images.values().flatten().map(|i| i.path.clone()).collect();
    attachments.extend(extra_attachments.iter().cloned());
    Ok(RenderedCard { kind, card, attachments })
}

fn format_ratio(r: f64) -> String {
    format!("{r:.2}×")
}

fn commit_link(github: &str, sha: &str) -> String {
    format!("[{}](https://github.com/{github}/commit/{sha})", crate::storage::short_sha(sha))
}

/// One regressed / recovered row for the alert card.
#[derive(Debug, Clone)]
pub struct AlertRow {
    /// Full row key, e.g. `salt_dynamic_gas/rex5_salt/sstore_100`.
    pub row_key: String,
    /// Rolling-median baseline ratio before this run.
    pub median: f64,
    /// This run's ratio.
    pub current: f64,
}

impl AlertRow {
    fn pct_change(&self) -> f64 {
        (self.current - self.median) / self.median * 100.0
    }
}

/// Params for the regression-alert / recovery card (same layout, different
/// color and wording).
#[derive(Debug, Clone)]
pub struct AlertCardParams<'a> {
    pub repo_name: &'a str,
    /// `owner/repo` for commit links.
    pub github: &'a str,
    pub sha: &'a str,
    /// Rows that just crossed the regression threshold.
    pub regressed: Vec<AlertRow>,
    /// Rows that just recovered back under it.
    pub recovered: Vec<AlertRow>,
    /// Charts to embed: the comparison charts plus dist plots of affected rows.
    pub images: Vec<ImageRef>,
    /// The threshold/window that produced this alert (footer text).
    pub threshold_pct: f64,
    pub window: usize,
}

pub fn render_alert_card(params: &AlertCardParams<'_>) -> anyhow::Result<RenderedCard> {
    if params.regressed.is_empty() && params.recovered.is_empty() {
        anyhow::bail!("alert card requested with no regressed or recovered rows");
    }
    let is_regression = !params.regressed.is_empty();
    let (kind, header_color, title) = if is_regression {
        (
            CardKind::RegressionAlert,
            "red",
            format!(
                "⚠️ {} 基准回归告警 @ {}",
                params.repo_name,
                crate::storage::short_sha(params.sha)
            ),
        )
    } else {
        (
            CardKind::Recovery,
            "green",
            format!(
                "✅ {} 基准回归恢复 @ {}",
                params.repo_name,
                crate::storage::short_sha(params.sha)
            ),
        )
    };

    let mut body = format!("**提交:** {}\n", commit_link(params.github, params.sha));
    if !params.regressed.is_empty() {
        body.push_str("\n**回归行**（相对 revm_pinned 的倍率 vs 滚动中位数）:\n");
        for row in &params.regressed {
            body.push_str(&format!(
                "- **{}**: {} → {} (**{:+.1}%**)\n",
                row.row_key,
                format_ratio(row.median),
                format_ratio(row.current),
                row.pct_change(),
            ));
        }
    }
    if !params.recovered.is_empty() {
        body.push_str("\n**已恢复**:\n");
        for row in &params.recovered {
            body.push_str(&format!(
                "- **{}**: 回到 {} (滚动中位数 {})\n",
                row.row_key,
                format_ratio(row.current),
                format_ratio(row.median),
            ));
        }
    }

    let vars = BTreeMap::from([
        ("header_color", header_color.to_string()),
        ("title", title),
        ("body_md", body),
        (
            "footer",
            format!(
                "mega-bench-reporter · 阈值 +{:.0}% vs 滚动中位数（窗口 {}）",
                params.threshold_pct, params.window
            ),
        ),
    ]);
    let images = BTreeMap::from([("charts", params.images.clone())]);
    render_template(TEMPLATE_REGRESSION_ALERT, kind, &vars, &images, &[])
}

/// One table row of the trend-digest card.
#[derive(Debug, Clone, Serialize)]
pub struct DigestTableRow {
    pub row_key: String,
    /// Ratio at the first / last commit of the digest window (None = row
    /// missing that run, rendered as `–`).
    pub first: Option<f64>,
    pub last: Option<f64>,
    /// Median ratio across the window.
    pub median: Option<f64>,
}

impl DigestTableRow {
    fn delta_pct(&self) -> Option<f64> {
        match (self.first, self.last) {
            (Some(f), Some(l)) if f != 0.0 => Some((l - f) / f * 100.0),
            _ => None,
        }
    }
}

fn opt_ratio(r: Option<f64>) -> String {
    r.map(format_ratio).unwrap_or_else(|| "–".to_string())
}

/// Params for the trend-digest card.
#[derive(Debug, Clone)]
pub struct DigestCardParams<'a> {
    pub repo_name: &'a str,
    pub github: &'a str,
    pub first_sha: &'a str,
    pub last_sha: &'a str,
    pub commit_count: usize,
    pub rows: Vec<DigestTableRow>,
    /// Bench targets that failed in any commit of the window (surfaced, not
    /// silently dropped).
    pub failed_targets: Vec<String>,
    pub trend_image: ImageRef,
}

pub fn render_digest_card(params: &DigestCardParams<'_>) -> anyhow::Result<RenderedCard> {
    let title =
        format!("📈 {} 基准趋势汇总（近 {} 次提交）", params.repo_name, params.commit_count);
    let summary = format!(
        "**区间:** {} … {}\n**基准行倍率均为相对 revm_pinned 的执行耗时比（越低越好）**",
        commit_link(params.github, params.first_sha),
        commit_link(params.github, params.last_sha),
    );

    let mut table = String::from("|基准行|区间中位|首次|最新|Δ 首→末|\n|---|---|---|---|---|\n");
    for row in &params.rows {
        let delta = row.delta_pct().map(|d| format!("{d:+.1}%")).unwrap_or_else(|| "–".to_string());
        table.push_str(&format!(
            "|{}|{}|{}|{}|{}|\n",
            row.row_key,
            opt_ratio(row.median),
            opt_ratio(row.first),
            opt_ratio(row.last),
            delta,
        ));
    }
    if !params.failed_targets.is_empty() {
        table.push_str(&format!(
            "\n⚠️ 区间内有失败的 bench target: {}\n",
            params.failed_targets.join(", ")
        ));
    }

    let vars = BTreeMap::from([
        ("title", title),
        ("summary_md", summary),
        ("table_md", table),
        ("footer", "mega-bench-reporter · 每 10 次提交自动汇总".to_string()),
    ]);
    let images = BTreeMap::from([("charts", vec![params.trend_image.clone()])]);
    render_template(TEMPLATE_TREND_DIGEST, CardKind::TrendDigest, &vars, &images, &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert_params<'a>(regressed: Vec<AlertRow>, recovered: Vec<AlertRow>) -> AlertCardParams<'a> {
        AlertCardParams {
            repo_name: "mega-evm",
            github: "megaeth-labs/mega-evm",
            sha: "abcdef0123456789",
            regressed,
            recovered,
            threshold_pct: 10.0,
            window: 20,
            images: vec![
                ImageRef::new(
                    "/data/mega-evm/commits/20260702-abcdef0/compare_table.png",
                    "对比图",
                ),
                ImageRef::new(
                    "/data/mega-evm/commits/20260702-abcdef0/dist_salt_dynamic_gas_sstore_100.png",
                    "分布图",
                ),
            ],
        }
    }

    fn card_text(card: &Value) -> String {
        serde_json::to_string(card).unwrap()
    }

    #[test]
    fn test_regression_alert_card() {
        let params = alert_params(
            vec![AlertRow {
                row_key: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
                median: 2.0,
                current: 2.3,
            }],
            vec![],
        );
        let rendered = render_alert_card(&params).unwrap();
        assert_eq!(rendered.kind, CardKind::RegressionAlert);
        let text = card_text(&rendered.card);
        assert!(text.contains("基准回归告警"));
        assert!(text.contains("abcdef0"));
        assert!(text.contains("+15.0%"));
        assert!(text.contains(r#""template":"red""#));
        // No unexpanded placeholders left.
        assert!(!text.contains("{{"), "unsubstituted placeholder in {text}");
        assert!(!text.contains("__images__"));
        // Both chart images expanded, in order, with ${image:} keys + attachments.
        assert!(text.contains("${image:compare_table.png}"));
        assert!(text.contains("${image:dist_salt_dynamic_gas_sstore_100.png}"));
        assert_eq!(rendered.attachments.len(), 2);
    }

    #[test]
    fn test_recovery_card_is_green_and_no_regression_wording() {
        let params = alert_params(
            vec![],
            vec![AlertRow {
                row_key: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
                median: 2.0,
                current: 2.02,
            }],
        );
        let rendered = render_alert_card(&params).unwrap();
        assert_eq!(rendered.kind, CardKind::Recovery);
        let text = card_text(&rendered.card);
        assert!(text.contains("基准回归恢复"));
        assert!(text.contains(r#""template":"green""#));
        assert!(!text.contains("回归行"));
    }

    #[test]
    fn test_alert_card_with_nothing_to_report_errors() {
        assert!(render_alert_card(&alert_params(vec![], vec![])).is_err());
    }

    #[test]
    fn test_trend_digest_card() {
        let params = DigestCardParams {
            repo_name: "mega-evm",
            github: "megaeth-labs/mega-evm",
            first_sha: "1111111aaaa",
            last_sha: "2222222bbbb",
            commit_count: 10,
            rows: vec![
                DigestTableRow {
                    row_key: "salt_dynamic_gas/rex5_salt/sstore_100".into(),
                    first: Some(2.0),
                    last: Some(2.1),
                    median: Some(2.05),
                },
                DigestTableRow {
                    row_key: "empty_transaction/rex5".into(),
                    first: None,
                    last: Some(1.2),
                    median: Some(1.2),
                },
            ],
            failed_targets: vec!["block_bench".into()],
            trend_image: ImageRef::new(
                "/data/mega-evm/digests/20260702-111..222/trend.png",
                "趋势图",
            ),
        };
        let rendered = render_digest_card(&params).unwrap();
        assert_eq!(rendered.kind, CardKind::TrendDigest);
        let text = card_text(&rendered.card);
        assert!(text.contains("趋势汇总（近 10 次提交）"));
        assert!(text.contains("2.05×"));
        assert!(text.contains("+5.0%"));
        // Missing first ratio renders a dash, not a crash or a bogus delta.
        assert!(text.contains("|–|1.20×|–|") || text.contains("|–|1.20×|–|\\n"));
        assert!(text.contains("block_bench"));
        assert!(text.contains("${image:trend.png}"));
        assert!(!text.contains("{{"));
        assert_eq!(rendered.attachments.len(), 1);
    }

    #[test]
    fn test_all_templates_are_valid_json() {
        for (name, template) in [
            ("regression_alert", TEMPLATE_REGRESSION_ALERT),
            ("trend_digest", TEMPLATE_TREND_DIGEST),
        ] {
            serde_json::from_str::<Value>(template)
                .unwrap_or_else(|e| panic!("template {name} is invalid JSON: {e}"));
        }
    }

    #[test]
    fn test_substituted_values_are_not_re_expanded() {
        // A row name containing a literal placeholder must come out verbatim,
        // not expand the card's other variables into the body.
        let params = AlertCardParams {
            repo_name: "mega-evm",
            github: "megaeth-labs/mega-evm",
            sha: "abcdef0123456789",
            regressed: vec![AlertRow {
                row_key: "g/rex5/x{{title}}y".into(),
                median: 1.0,
                current: 1.2,
            }],
            recovered: vec![],
            threshold_pct: 10.0,
            window: 20,
            images: vec![],
        };
        let rendered = render_alert_card(&params).unwrap();
        let text = card_text(&rendered.card);
        assert!(text.contains(r"x{{title}}y"), "placeholder in value was re-expanded: {text}");
    }

    #[test]
    fn test_substitution_handles_json_meta_characters() {
        // A value containing quotes/backslashes/newlines must survive the
        // template round-trip intact (substitution is on decoded strings).
        let params = AlertCardParams {
            repo_name: r#"we"ird\repo"#,
            github: "megaeth-labs/mega-evm",
            sha: "abcdef0123456789",
            regressed: vec![AlertRow { row_key: "g/s/w".into(), median: 1.0, current: 1.2 }],
            recovered: vec![],
            threshold_pct: 10.0,
            window: 20,
            images: vec![],
        };
        let rendered = render_alert_card(&params).unwrap();
        let title = rendered.card["header"]["title"]["content"].as_str().unwrap();
        assert!(title.contains(r#"we"ird\repo"#));
    }
}
