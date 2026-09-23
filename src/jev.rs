use crate::types::{Candidate, EvaluatedCandidate, JevResponse};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Live Jev is served through OpenRouter's alpha Decisions API; the request and
// response schema are identical to the direct TypeSafe endpoint (same
// state/questions body, same {"answers": {...}} envelope). Point JEV_API_URL /
// JEV_MODEL at a direct/self-hosted endpoint to bypass OpenRouter.
const DEFAULT_JEV_URL: &str = "https://openrouter.ai/api/alpha/decisions";
const DEFAULT_JEV_MODEL: &str = "~typesafe/jev-latest";
/// Shared key file, same location the jev-decision skill reads (chmod 600).
const KEY_FILE: &str = ".config/openrouter/key";

fn jev_url() -> String {
    std::env::var("JEV_API_URL").unwrap_or_else(|_| DEFAULT_JEV_URL.to_string())
}

fn jev_model() -> String {
    std::env::var("JEV_MODEL").unwrap_or_else(|_| DEFAULT_JEV_MODEL.to_string())
}

/// Resolve the Jev API key. Precedence: an explicit env var
/// (TYPESAFE_API_KEY or OPENROUTER_API_KEY) first, then the shared key file
/// ~/.config/openrouter/key. Mirrors the token-discovery idiom in search.rs.
pub fn resolve_api_key() -> Result<String, String> {
    for var in ["TYPESAFE_API_KEY", "OPENROUTER_API_KEY"] {
        if let Ok(k) = std::env::var(var) {
            let k = k.trim();
            if !k.is_empty() {
                return Ok(k.to_string());
            }
        }
    }
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let path = std::path::Path::new(&home).join(KEY_FILE);
    let key = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "no Jev API key: set TYPESAFE_API_KEY or OPENROUTER_API_KEY, or create {} ({})",
            path.display(),
            e
        )
    })?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(format!("empty Jev API key in {}", path.display()));
    }
    Ok(key)
}

// ponytail: Jev scores are deterministic per (query, candidate set); cache 60s
// so repeated/MCP calls skip the ~1.1s API floor entirely.
const EVAL_CACHE_TTL: Duration = Duration::from_secs(60);
type EvalCache = HashMap<String, (Instant, Vec<EvaluatedCandidate>)>;
static EVAL_CACHE: Mutex<Option<EvalCache>> = Mutex::new(None);

pub fn evaluate_candidates(
    query: &str,
    candidates: Vec<Candidate>,
    api_key: &str,
) -> Result<Vec<EvaluatedCandidate>, String> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    // Deterministic cache key: query + candidate ids
    let mut ids: Vec<&str> = candidates.iter().map(|c| c.id.as_str()).collect();
    ids.sort_unstable();
    let key = format!("{}|{}", query.trim().to_lowercase(), ids.join(","));

    if let Ok(guard) = EVAL_CACHE.lock() {
        if let Some(map) = guard.as_ref() {
            if let Some((ts, hit)) = map.get(&key) {
                if ts.elapsed() < EVAL_CACHE_TTL {
                    return Ok(hit.clone());
                }
            }
        }
    }

    let evaluated = if candidates.len() <= 3 {
        evaluate_via_api(query, &candidates, api_key)?
    } else {
        // Jev drops questions past ~15 per call: chunk, fan out per chunk, merge.
        let mut all = Vec::new();
        for chunk in candidates.chunks(3) {
            all.extend(evaluate_via_api(query, chunk, api_key)?);
        }
        all
    };

    if let Ok(mut guard) = EVAL_CACHE.lock() {
        let map = guard.get_or_insert_with(EvalCache::new);
        map.retain(|_, (ts, _)| ts.elapsed() < EVAL_CACHE_TTL);
        map.insert(key, (Instant::now(), evaluated.clone()));
    }

    Ok(evaluated)
}

fn evaluate_via_api(
    query: &str,
    candidates: &[Candidate],
    api_key: &str,
) -> Result<Vec<EvaluatedCandidate>, String> {
    // Build Choice criteria for best_match
    let mut choice_criteria = serde_json::Map::new();
    let mut questions = serde_json::Map::new();

    for (idx, c) in candidates.iter().enumerate() {
        choice_criteria.insert(
            c.id.clone(),
            json!(format!("{}: {}", c.name, c.description)),
        );

        // Per-candidate Fit Score question (criteria is a list of levels)
        questions.insert(
            format!("fit_{}", idx),
            json!({
                "type": "score",
                "instructions": format!("Relevance of '{}' to '{}'?", c.name, query),
                "criteria": [
                    "Unrelated or completely different functional domain",
                    "Loosely related or missing core requested features/language",
                    "Strong match satisfying most constraints",
                    "Exact architectural and functional match"
                ]
            }),
        );

        // Per-candidate Docs Score question
        questions.insert(
            format!("doc_{}", idx),
            json!({
                "type": "score",
                "instructions": format!("Docs quality of '{}'?", c.name),
                "criteria": [
                    "No usable description",
                    "One-line or vague description",
                    "Clear description with purpose",
                    "Excellent docs with examples and links"
                ]
            }),
        );

        // Per-candidate Modern/Active Maintenance Noul question
        questions.insert(
            format!("modern_{}", idx),
            json!({
                "type": "noul",
                "instructions": format!("Is '{}' actively maintained?", c.name),
                "criteria": {
                    "true": "Actively maintained with modern tooling",
                    "false": "Deprecated, abandoned, or legacy code"
                }
            }),
        );
    }

    // Best match Choice question
    questions.insert(
        "best_match".to_string(),
        json!({
            "type": "choice",
            "instructions": format!("Single best for '{}'?", query),
            "criteria": choice_criteria
        }),
    );

    let payload = json!({
        "model": jev_model(),
        "state": {
            "query": query,
            "candidate_count": candidates.len(),
            // Trim derivable fields (install_cmd, url, ecosystem) to cut tokens
            // and reduce context rot: Jev only needs what it judges on.
            "candidates": candidates.iter().map(candidate_state).collect::<Vec<_>>()
        },
        "questions": questions
    });

    let response = match ureq::post(&jev_url())
        .set("Authorization", &format!("Bearer {}", api_key))
        .set("Content-Type", "application/json")
        .timeout(Duration::from_secs(8))
        .send_json(payload)
    {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            let err_body = resp.into_string().unwrap_or_default();
            return Err(format!("TypeSafe Jev API HTTP {}: {}", code, err_body));
        }
        Err(e) => return Err(format!("Failed to call TypeSafe Jev API: {}", e)),
    };

    let jev_res: JevResponse = response
        .into_json()
        .map_err(|e| format!("Failed to parse Jev API response: {}", e))?;

    let answers = jev_res
        .answers
        .ok_or_else(|| "Jev response missing answers".to_string())?;
    let best_match_id = answers
        .get("best_match")
        .and_then(|v| v["choice"].as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            eprintln!("Warning: Jev best_match missing, no top pick flagged.");
            String::new()
        });

    let mut evaluated = Vec::new();

    for (idx, c) in candidates.iter().enumerate() {
        // Composite fit: relevance, maturity, docs combined with code weights.
        // A dropped dimension skips the candidate loudly, never defaults.
        let prefixes = ["fit", "doc"];
        let mut fit_score = 0.0;
        let mut confidence = 1.0f64;
        let mut ok = true;
        for (i, w) in crate::policy::FIT_WEIGHTS.iter().map(|(_, w)| w).enumerate() {
            let ans = answers.get(&format!("{}_{}", prefixes[i], idx));
            match ans.and_then(|v| v["score"].as_f64()).filter(|s| (0.0..=3.0).contains(s)) {
                Some(s) => fit_score += s * w,
                None => {
                    eprintln!("Warning: Jev {}_{} for '{}' malformed, skipping.", prefixes[i], idx, c.id);
                    ok = false;
                    break;
                }
            }
            confidence = confidence.min(
                ans.and_then(|v| v["confidence"].as_f64()).unwrap_or(0.0),
            );
        }
        if !ok {
            continue;
        }
        let modern_ans = answers.get(&format!("modern_{}", idx));
        let is_modern = match modern_ans.and_then(|v| v["noul"].as_f64()).filter(|n| (0.0..=1.0).contains(n)) {
            Some(n) => n,
            None => {
                eprintln!("Warning: Jev modern_{} for '{}' malformed, skipping.", idx, c.id);
                continue;
            }
        };

        let is_best = c.id == best_match_id;
        // Date and maintainer math belongs in host code, never in Jev (typesafe rule #2).
        let weighted_rank = fit_score * confidence * rank_penalty(c);

        evaluated.push(EvaluatedCandidate {
            candidate: c.clone(),
            fit_score,
            is_modern,
            confidence,
            weighted_rank,
            is_best_match: is_best,
        });
    }

    // Sort descending by confidence-weighted score
    evaluated.sort_by(|a, b| {
        b.weighted_rank
            .partial_cmp(&a.weighted_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(evaluated)
}

/// Drop weak matches below policy floors so low-quality picks never reach the user.
/// What Jev sees per candidate. Derivable fields (install_cmd, url) are trimmed;
/// registry-specific facts are added only when the registry supplies them.
fn candidate_state(c: &crate::types::Candidate) -> serde_json::Value {
    let mut state = json!({
        "id": c.id,
        "name": c.name,
        "description": c.description,
        "stars": c.stars,
        "downloads": c.downloads,
        "license": c.license,
        "updated_at": c.updated_at,
        "pushed_at": c.pushed_at,
        "language": c.language,
        "topics": c.topics
    });
    if c.ecosystem == "nixpkgs" {
        state["stars_meaning"] = json!("number of Linux distributions that package it");
    }
    if let Some(n) = c.maintainers {
        state["maintainers"] = json!(n);
    }
    state
}

/// Host-computed rank multiplier: stale (no push/update in STALE_DAYS) and orphaned
/// (registry says zero maintainers) each cost weight. Never delegated to Jev.
fn rank_penalty(c: &crate::types::Candidate) -> f64 {
    let stale = is_stale_180d(if c.pushed_at.is_empty() {
        &c.updated_at
    } else {
        &c.pushed_at
    });
    let mut penalty = 1.0;
    if stale {
        penalty *= crate::policy::STALE_PENALTY;
    }
    if c.maintainers == Some(0) {
        penalty *= crate::policy::ORPHAN_PENALTY;
    }
    penalty
}

pub fn filter_weak(mut evaluated: Vec<EvaluatedCandidate>) -> Vec<EvaluatedCandidate> {
    evaluated.retain(|e| e.fit_score >= crate::policy::MIN_FIT && e.confidence >= crate::policy::MIN_CONFIDENCE);
    evaluated
}

/// True if an ISO-8601 date ("YYYY-MM-DD...") is older than 180 days.
/// Julian-day arithmetic, no chrono dependency, deterministic.
fn is_stale_180d(iso: &str) -> bool {
    let date = iso.split('T').next().unwrap_or("");
    if date.len() < 10 {
        return false;
    }
    let y: i64 = date[0..4].parse().unwrap_or(0);
    let m: i64 = date[5..7].parse().unwrap_or(0);
    let d: i64 = date[8..10].parse().unwrap_or(0);
    if y == 0 || m == 0 || d == 0 {
        return false;
    }
    let days = julian_day(y, m, d);
    // JDN of unix epoch is 2440588; now_days counts from epoch.
    let now_days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|s| s.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    now_days - (days - 2440588) > crate::policy::STALE_DAYS
}

fn julian_day(y: i64, m: i64, d: i64) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let a = y / 100;
    let b = 2 - a + a / 4;
    (36525 * (y + 4716)) / 100 + (306 * (m + 1)) / 10 + d + b - 1524
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_boundary() {
        assert!(!is_stale_180d(""));
        assert!(!is_stale_180d("not-a-date"));
        assert!(!is_stale_180d("2026-09-21T00:00:00Z"));
        assert!(is_stale_180d("2020-01-01T00:00:00Z"));
        assert!(is_stale_180d("2020-01-01"));
    }

    #[test]
    fn rank_penalty_compounds_stale_and_orphan() {
        let mk = |updated: &str, maintainers: Option<u32>| crate::types::Candidate {
            id: "x".into(),
            name: "x".into(),
            description: "".into(),
            url: "".into(),
            stars: 0,
            downloads: 0,
            license: "".into(),
            updated_at: updated.into(),
            pushed_at: "".into(),
            language: "".into(),
            topics: vec![],
            ecosystem: "nixpkgs".into(),
            install_cmd: "".into(),
            maintainers,
        };
        let (stale, orphan) = (crate::policy::STALE_PENALTY, crate::policy::ORPHAN_PENALTY);
        assert_eq!(rank_penalty(&mk("", None)), 1.0, "unknown is neutral");
        assert_eq!(rank_penalty(&mk("", Some(2))), 1.0);
        assert_eq!(rank_penalty(&mk("", Some(0))), orphan);
        assert_eq!(rank_penalty(&mk("2020-01-01", Some(0))), stale * orphan);
        let state = candidate_state(&mk("", Some(0)));
        assert_eq!(state["maintainers"], 0);
        assert!(state["stars_meaning"].is_string());
        assert!(candidate_state(&mk("", None)).get("maintainers").is_none());
    }

    #[test]
    fn weak_filter_thresholds() {
        let mk = |fit: f64, conf: f64| EvaluatedCandidate {
            candidate: crate::types::Candidate {
                id: "x".into(),
                name: "x".into(),
                description: "".into(),
                url: "".into(),
                stars: 0,
                downloads: 0,
                license: "".into(),
                updated_at: "".into(),
                pushed_at: "".into(),
                language: "".into(),
                topics: vec![],
                ecosystem: "".into(),
                install_cmd: "".into(),
                maintainers: None,
            },
            fit_score: fit,
            is_modern: 1.0,
            confidence: conf,
            weighted_rank: fit * conf,
            is_best_match: false,
        };
        let out = filter_weak(vec![mk(2.5, 0.9), mk(1.4, 0.9), mk(2.5, 0.4)]);
        assert_eq!(out.len(), 1);
    }
}
