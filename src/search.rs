use crate::policy;
use crate::types::Candidate;
use serde::Deserialize;
use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ponytail: whole-map TTL cache (no LRU eviction). Memory is bounded by distinct
// queries per 60s window; switch to true LRU if MCP sessions show unbounded growth.
const CACHE_TTL: Duration = Duration::from_secs(60);
type Cache = HashMap<String, (Instant, Vec<Candidate>)>;
static SEARCH_CACHE: Mutex<Option<Cache>> = Mutex::new(None);

fn cache_get(key: &str) -> Option<Vec<Candidate>> {
    let guard = SEARCH_CACHE.lock().ok()?;
    let map = guard.as_ref()?;
    let (ts, hits) = map.get(key)?;
    if ts.elapsed() > CACHE_TTL {
        return None;
    }
    Some(hits.clone())
}

fn cache_put(key: &str, value: &[Candidate]) {
    if let Ok(mut guard) = SEARCH_CACHE.lock() {
        let map = guard.get_or_insert_with(Cache::new);
        map.retain(|_, (ts, _)| ts.elapsed() < CACHE_TTL);
        map.insert(key.to_string(), (Instant::now(), value.to_vec()));
    }
}

pub fn get_github_token() -> Option<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            return Some(token.trim().to_string());
        }
    }
    if let Ok(token) = std::env::var("GH_TOKEN") {
        if !token.trim().is_empty() {
            return Some(token.trim().to_string());
        }
    }
    // Fallback: query gh CLI
    if let Ok(output) = Command::new("gh").args(["auth", "token"]).output() {
        if output.status.success() {
            let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

/// Percent-encode a query string. Keeps unreserved + space-as-plus.
fn encode_query(query: &str) -> String {
    let mut out = String::new();
    for b in query.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub fn search_github(query: &str, limit: usize) -> Result<Vec<Candidate>, String> {
    let mut candidates = Vec::new();
    let encoded_query = encode_query(query);
    let url = format!(
        "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page={}",
        encoded_query,
        limit.max(4)
    );

    let mut request = ureq::get(&url)
        .set("User-Agent", "jev-scout/0.1.0")
        .set("Accept", "application/vnd.github.v3+json")
        .timeout(Duration::from_secs(5));

    if let Some(token) = get_github_token() {
        request = request.set("Authorization", &format!("Bearer {}", token));
    }

    match request.call() {
        Err(ureq::Error::Status(403, _)) => {
            return Err("GitHub search rate limited (403). Run `gh auth login`, then retry.".to_string())
        }
        Err(ureq::Error::Status(429, _)) => {
            return Err("GitHub search rate limited (429). Wait a minute, then retry.".to_string())
        }
        Err(ureq::Error::Transport(e)) if e.to_string().contains("timed out") => {
            return Err("GitHub search timed out. Retry on a better connection.".to_string())
        }
        Err(e) => return Err(format!("GitHub search failed: {}", e)),
        Ok(response) => {
            let json: serde_json::Value = response
                .into_json()
                .map_err(|e| format!("GitHub returned bad JSON: {}", e))?;
            let items = json["items"]
                .as_array()
                .ok_or_else(|| "GitHub response has no items".to_string())?;
            for item in items.iter().take(limit) {
                let full_name = item["full_name"].as_str().unwrap_or("").to_string();
                let name = item["name"].as_str().unwrap_or("").to_string();
                let description = item["description"]
                    .as_str()
                    .unwrap_or("No description provided")
                    .to_string();
                let html_url = item["html_url"].as_str().unwrap_or("").to_string();
                let stars = item["stargazers_count"].as_u64().unwrap_or(0);
                let license = item["license"]["spdx_id"]
                    .as_str()
                    .unwrap_or("None")
                    .to_string();
                let updated_at = item["updated_at"].as_str().unwrap_or("").to_string();

                if !full_name.is_empty() {
                    let topics: Vec<String> = item["topics"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    candidates.push(Candidate {
                        id: full_name.clone(),
                        name,
                        description,
                        url: html_url,
                        stars,
                        downloads: 0,
                        license,
                        updated_at,
                        pushed_at: item["pushed_at"].as_str().unwrap_or("").to_string(),
                        language: item["language"].as_str().unwrap_or("").to_string(),
                        topics,
                        ecosystem: "github".to_string(),
                        install_cmd: format!("gh repo clone {}", full_name),
                    });
                }
            }
        }
    }

    Ok(candidates)
}

pub fn search_crates_io(query: &str, limit: usize) -> Result<Vec<Candidate>, String> {
    let mut candidates = Vec::new();
    let encoded_query = encode_query(query);
    let url = format!(
        "https://crates.io/api/v1/crates?q={}&per_page={}",
        encoded_query,
        limit.max(4)
    );

    let request = ureq::get(&url)
        .set("User-Agent", "jev-scout/0.1.0 (akashpriyadarshii)")
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(5));

    match request.call() {
        Err(ureq::Error::Status(429, _)) => {
            return Err("crates.io rate limited (429). Wait a minute, then retry.".to_string())
        }
        Err(ureq::Error::Transport(e)) if e.to_string().contains("timed out") => {
            return Err("crates.io timed out. Retry on a better connection.".to_string())
        }
        Err(e) => return Err(format!("crates.io search failed: {}", e)),
        Ok(response) => {
            let json: serde_json::Value = response
                .into_json()
                .map_err(|e| format!("crates.io returned bad JSON: {}", e))?;
            let crates = json["crates"]
                .as_array()
                .ok_or_else(|| "crates.io response has no crates".to_string())?;
            for item in crates.iter().take(limit) {
                let name = item["name"].as_str().unwrap_or("").to_string();
                let description = item["description"]
                    .as_str()
                    .unwrap_or("No description provided")
                    .to_string();
                let downloads = item["downloads"].as_u64().unwrap_or(0);
                let updated_at = item["updated_at"].as_str().unwrap_or("").to_string();
                let url = format!("https://crates.io/crates/{}", name);
                let license = item["license"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("Unknown")
                    .to_string();

                if !name.is_empty() {
                    candidates.push(Candidate {
                        id: format!("crate:{}", name),
                        name: name.clone(),
                        description,
                        url,
                        stars: 0,
                        downloads,
                        license,
                        updated_at,
                        pushed_at: "".to_string(),
                        language: "Rust".to_string(),
                        topics: Vec::new(),
                        ecosystem: "crates.io".to_string(),
                        install_cmd: format!("cargo add {}", name),
                    });
                }
            }
        }
    }

    Ok(candidates)
}

/// DuckDuckGo HTML web results: docs, blogs, tutorials beyond code homes.
/// Empty parse = loud error, never fake rows (bot challenges return 200).
pub fn search_duckduckgo(query: &str, limit: usize) -> Result<Vec<Candidate>, String> {
    let mut candidates = Vec::new();
    let form = format!("q={}&kl=us-en", encode_query(query));
    let html = ureq::post("https://html.duckduckgo.com/html/")
        .set("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36")
        .set("Content-Type", "application/x-www-form-urlencoded")
        .timeout(Duration::from_secs(8))
        .send_string(&form)
        .map_err(|e| format!("DuckDuckGo search failed: {}", e))?
        .into_string()
        .map_err(|e| format!("DuckDuckGo returned bad body: {}", e))?;

    let lower = html.to_lowercase();
    if lower.contains("anomaly") || lower.contains("captcha") {
        return Err("DuckDuckGo bot challenge, no results recorded.".to_string());
    }

    for block in html.split("result__title").skip(1).take(limit) {
        if let Some(c) = parse_ddg_block(block) {
            candidates.push(c);
        }
    }

    if candidates.is_empty() {
        return Err("DuckDuckGo returned no parseable results, possibly blocked.".to_string());
    }
    Ok(candidates)
}

fn parse_ddg_block(block: &str) -> Option<Candidate> {
    let href = block.find("href=\"").and_then(|s| {
        let rest = &block[s + 6..];
        rest.find('"').map(|e| rest[..e].to_string())
    })?;
    let url = if let Some(pos) = href.find("uddg=") {
        let rem = &href[pos + 5..];
        rem[..rem.find('&').unwrap_or(rem.len())].to_string()
    } else if href.starts_with("http") {
        href
    } else {
        return None;
    };
    let title = block
        .find('>')
        .map(|s| {
            let rest = &block[s + 1..];
            rest[..rest.find('<').unwrap_or(rest.len())]
                .trim()
                .to_string()
        })
        .unwrap_or_default();
    if title.is_empty() || url.is_empty() {
        return None;
    }
    Some(Candidate {
        id: format!("web:{}", url),
        name: title,
        description: "Web result via DuckDuckGo".to_string(),
        url: url.clone(),
        stars: 0,
        downloads: 0,
        license: "Web".to_string(),
        updated_at: "".to_string(),
        pushed_at: "".to_string(),
        language: "".to_string(),
        topics: Vec::new(),
        ecosystem: "web".to_string(),
        install_cmd: url,
    })
}

/// Map a ureq failure to one loud, actionable line.
fn upstream_error(source: &str, e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(429, _) => format!("{} rate limited (429). Wait a minute, then retry.", source),
        ureq::Error::Status(code, _) => format!("{} returned HTTP {}", source, code),
        ureq::Error::Transport(t) if t.to_string().contains("timed out") => {
            format!("{} timed out. Retry on a better connection.", source)
        }
        other => format!("{} search failed: {}", source, other),
    }
}

/// Lowercased alphanumeric query words, the unit MELPA matching works on.
fn query_terms(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------- MELPA

#[derive(Debug, Deserialize)]
struct MelpaEntry {
    ver: Vec<i64>,
    desc: Option<String>,
    props: Option<MelpaProps>,
}

#[derive(Debug, Default, Deserialize)]
struct MelpaProps {
    url: Option<String>,
    keywords: Option<Vec<String>>,
}

struct MelpaIndex {
    packages: HashMap<String, MelpaEntry>,
    downloads: HashMap<String, u64>,
}

// ponytail: MELPA has no search endpoint, so the whole 2.7MB archive.json is the
// index. Held in memory for CACHE_TTL; a cold CLI run always pays the download.
static MELPA_CACHE: Mutex<Option<(Instant, Arc<MelpaIndex>)>> = Mutex::new(None);

fn fetch_json<T: serde::de::DeserializeOwned>(source: &str, url: &str) -> Result<T, String> {
    ureq::get(url)
        .set("User-Agent", "jev-scout/0.1.0")
        .timeout(Duration::from_secs(8))
        .call()
        .map_err(|e| upstream_error(source, e))?
        .into_json()
        .map_err(|e| format!("{} returned bad JSON: {}", source, e))
}

fn melpa_index() -> Result<Arc<MelpaIndex>, String> {
    if let Ok(guard) = MELPA_CACHE.lock() {
        if let Some((ts, index)) = guard.as_ref() {
            if ts.elapsed() < CACHE_TTL {
                return Ok(Arc::clone(index));
            }
        }
    }
    let counts = std::thread::spawn(|| {
        fetch_json::<HashMap<String, u64>>("MELPA downloads", "https://melpa.org/download_counts.json")
    });
    let packages = fetch_json::<HashMap<String, MelpaEntry>>("MELPA", "https://melpa.org/archive.json")?;
    // Download counts only rank ties and fill the display column: warn, don't fail.
    let downloads = counts
        .join()
        .map_err(|_| "MELPA downloads thread panicked".to_string())
        .and_then(|r| r)
        .unwrap_or_else(|e| {
            eprintln!("Warning: {}", e);
            HashMap::new()
        });
    let index = Arc::new(MelpaIndex { packages, downloads });
    if let Ok(mut guard) = MELPA_CACHE.lock() {
        *guard = Some((Instant::now(), Arc::clone(&index)));
    }
    Ok(index)
}

/// Relevance of one MELPA package to the query. 0 means "not a match".
/// Higher wins; ties break on download count.
fn melpa_match_score(terms: &[String], name: &str, desc: &str, keywords: &[String]) -> u32 {
    let name = name.to_lowercase();
    let desc_words = query_terms(desc);
    let mut score = 0;
    for term in terms.iter().filter(|t| !policy::MELPA_STOPWORDS.contains(&t.as_str())) {
        let hit = melpa_term_points(term, &name, &desc_words, keywords);
        if hit > 0 {
            score += hit + policy::MELPA_COVERAGE_BONUS;
        }
    }
    score
}

/// Best single placement of one query word; name beats keyword beats description.
fn melpa_term_points(term: &str, name: &str, desc_words: &[String], keywords: &[String]) -> u32 {
    if name.split('-').any(|seg| seg == term) {
        policy::MELPA_NAME_SEGMENT
    } else if term.len() >= 3 && name.contains(term) {
        policy::MELPA_NAME_SUBSTRING
    } else if keywords.iter().any(|k| k.eq_ignore_ascii_case(term)) {
        policy::MELPA_KEYWORD
    } else if desc_words.iter().any(|w| w == term) {
        policy::MELPA_DESCRIPTION
    } else {
        0
    }
}

/// MELPA snapshot versions are [YYYYMMDD, HHMM]; turn the first into an ISO date.
fn melpa_date(ver: &[i64]) -> String {
    match ver.first() {
        Some(&d) if (19000101..=99991231).contains(&d) => {
            format!("{:04}-{:02}-{:02}T00:00:00Z", d / 10000, d / 100 % 100, d % 100)
        }
        _ => String::new(),
    }
}

fn melpa_candidate(name: &str, entry: &MelpaEntry, downloads: u64) -> Candidate {
    let props = entry.props.as_ref();
    Candidate {
        id: format!("melpa:{}", name),
        name: name.to_string(),
        description: entry
            .desc
            .clone()
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| "No description provided".to_string()),
        url: props
            .and_then(|p| p.url.clone())
            .unwrap_or_else(|| format!("https://melpa.org/#/{}", name)),
        stars: 0,
        downloads,
        license: "Unknown".to_string(),
        updated_at: melpa_date(&entry.ver),
        pushed_at: String::new(),
        language: "Emacs Lisp".to_string(),
        topics: props.and_then(|p| p.keywords.clone()).unwrap_or_default(),
        ecosystem: "melpa".to_string(),
        install_cmd: format!("M-x package-install RET {}", name),
    }
}

/// Score every package locally, keep matches, best first.
fn rank_melpa(index: &MelpaIndex, terms: &[String], limit: usize) -> Vec<Candidate> {
    let mut scored: Vec<(u32, u64, &str, &MelpaEntry)> = index
        .packages
        .iter()
        .filter_map(|(name, entry)| {
            let props = entry.props.as_ref();
            let keywords = props.and_then(|p| p.keywords.as_deref()).unwrap_or(&[]);
            let desc = entry.desc.as_deref().unwrap_or("");
            let score = melpa_match_score(terms, name, desc, keywords);
            let downloads = index.downloads.get(name).copied().unwrap_or(0);
            (score > 0).then_some((score, downloads, name.as_str(), entry))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(b.2)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, downloads, name, entry)| melpa_candidate(name, entry, downloads))
        .collect()
}

pub fn search_melpa(query: &str, limit: usize) -> Result<Vec<Candidate>, String> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Err("MELPA search needs at least one alphanumeric word".to_string());
    }
    let index = melpa_index()?;
    Ok(rank_melpa(&index, &terms, limit))
}

// ---------------------------------------------------------------- nixpkgs

const NIX_BACKEND: &str = "https://search.nixos.org/backend";
/// Public read-only credentials, shipped in search.nixos.org's own frontend bundle.
const NIX_AUTH: &str = "Basic YVdWU0FMWHBadjpYOGdQSG56TDUyd0ZFZWt1eHNmUTljU2g=";

// ponytail: alias is resolved once per process per channel. It rotates only on
// search.nixos.org schema bumps (months apart), so a long MCP session is safe.
static NIX_ALIAS: Mutex<Option<(String, String)>> = Mutex::new(None);

fn nix_channel() -> String {
    std::env::var("JEV_NIX_CHANNEL")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "unstable".to_string())
}

/// From `_cat/aliases` output, pick the highest-numbered `latest-N-nixos-<channel>`.
fn pick_nix_alias(cat_output: &str, channel: &str) -> Option<String> {
    let suffix = format!("nixos-{}", channel);
    cat_output
        .lines()
        .filter_map(|line| {
            let alias = line.split_whitespace().next()?;
            let (n, rest) = alias.strip_prefix("latest-")?.split_once('-')?;
            (rest == suffix).then_some((n.parse::<u32>().ok()?, alias))
        })
        .max_by_key(|(n, _)| *n)
        .map(|(_, alias)| alias.to_string())
}

fn resolve_nix_alias(channel: &str) -> Result<String, String> {
    if let Ok(guard) = NIX_ALIAS.lock() {
        if let Some((cached_channel, alias)) = guard.as_ref() {
            if cached_channel == channel {
                return Ok(alias.clone());
            }
        }
    }
    let url = format!("{}/_cat/aliases/latest-*-nixos-{}?h=alias", NIX_BACKEND, channel);
    let body = ureq::get(&url)
        .set("Authorization", NIX_AUTH)
        .timeout(Duration::from_secs(8))
        .call()
        .map_err(|e| upstream_error("nixpkgs", e))?
        .into_string()
        .map_err(|e| format!("nixpkgs returned bad body: {}", e))?;
    let alias = pick_nix_alias(&body, channel).ok_or_else(|| {
        format!("nixpkgs channel '{}' not found (set JEV_NIX_CHANNEL, e.g. unstable or 26.05)", channel)
    })?;
    if let Ok(mut guard) = NIX_ALIAS.lock() {
        *guard = Some((channel.to_string(), alias.clone()));
    }
    Ok(alias)
}

fn first_str(v: &serde_json::Value) -> Option<&str> {
    v.as_array()?.first()?.as_str()
}

fn nix_candidate(src: &serde_json::Value) -> Option<Candidate> {
    let attr = src["package_attr_name"].as_str().filter(|a| !a.is_empty())?;
    let version = src["package_pversion"].as_str().unwrap_or("");
    let licenses: Vec<&str> = src["package_license_set"]
        .as_array()
        .map(|a| a.iter().filter_map(|l| l.as_str()).collect())
        .unwrap_or_default();
    Some(Candidate {
        id: format!("nix:{}", attr),
        name: attr.to_string(),
        description: src["package_description"]
            .as_str()
            .filter(|d| !d.is_empty())
            .unwrap_or("No description provided")
            .to_string(),
        url: first_str(&src["package_homepage"])
            .map(str::to_string)
            .unwrap_or_else(|| format!("https://search.nixos.org/packages?query={}", encode_query(attr))),
        stars: 0,
        downloads: 0,
        license: if licenses.is_empty() { "Unknown".to_string() } else { licenses.join(", ") },
        updated_at: String::new(),
        pushed_at: String::new(),
        language: String::new(),
        topics: if version.is_empty() { Vec::new() } else { vec![version.to_string()] },
        ecosystem: "nixpkgs".to_string(),
        install_cmd: format!("nix shell nixpkgs#{}", attr),
    })
}

fn parse_nix_hits(json: &serde_json::Value, limit: usize) -> Result<Vec<Candidate>, String> {
    let hits = json["hits"]["hits"]
        .as_array()
        .ok_or_else(|| "nixpkgs response has no hits".to_string())?;
    Ok(hits
        .iter()
        .filter_map(|h| nix_candidate(&h["_source"]))
        .take(limit)
        .collect())
}

pub fn search_nixpkgs(query: &str, limit: usize) -> Result<Vec<Candidate>, String> {
    let alias = resolve_nix_alias(&nix_channel())?;
    // Same query shape search.nixos.org itself sends.
    let body = serde_json::json!({
        "size": limit.max(4),
        "_source": ["package_attr_name", "package_pversion", "package_description",
                    "package_homepage", "package_license_set"],
        "query": { "bool": {
            "filter": [{ "term": { "type": "package" } }],
            "must": [{ "multi_match": {
                "query": query,
                "type": "cross_fields",
                "operator": "or",
                "fields": ["package_attr_name^9", "package_pname^6",
                           "package_description^1.3", "package_longDescription^1"]
            }}]
        }}
    });
    let json: serde_json::Value = ureq::post(&format!("{}/{}/_search", NIX_BACKEND, alias))
        .set("Authorization", NIX_AUTH)
        .timeout(Duration::from_secs(8))
        .send_json(body)
        .map_err(|e| upstream_error("nixpkgs", e))?
        .into_json()
        .map_err(|e| format!("nixpkgs returned bad JSON: {}", e))?;
    parse_nix_hits(&json, limit)
}

type SearchFn = fn(&str, usize) -> Result<Vec<Candidate>, String>;

/// Every registry jev-scout can ground on: (canonical name, accepted aliases, fetcher).
/// Adding an ecosystem is one row here; dispatch, `all` fan-out, CLI help and the
/// MCP enum all derive from this table.
const SOURCES: &[(&str, &[&str], SearchFn)] = &[
    ("github", &["github"], search_github),
    ("crates", &["crates", "crates.io", "rust"], search_crates_io),
    ("web", &["web"], search_duckduckgo),
    ("emacs", &["emacs", "melpa", "elisp"], search_melpa),
    ("nix", &["nix", "nixos", "nixpkgs"], search_nixpkgs),
];

/// Canonical ecosystem names, in fan-out order (excludes `all`).
pub fn ecosystem_names() -> impl Iterator<Item = &'static str> {
    SOURCES.iter().map(|(name, _, _)| *name)
}

/// True for `all` or any known alias. Case-insensitive.
pub fn is_valid_ecosystem(ecosystem: &str) -> bool {
    ecosystem.eq_ignore_ascii_case("all") || find_source(ecosystem).is_some()
}

fn find_source(ecosystem: &str) -> Option<SearchFn> {
    let wanted = ecosystem.to_lowercase();
    SOURCES
        .iter()
        .find(|(_, aliases, _)| aliases.contains(&wanted.as_str()))
        .map(|(_, _, f)| *f)
}

/// Run one source; an upstream failure is a loud warning plus zero rows, never fake rows.
fn run_logged(search: SearchFn, query: &str, limit: usize) -> Vec<Candidate> {
    search(query, limit).unwrap_or_else(|e| {
        eprintln!("Warning: {}", e);
        Vec::new()
    })
}

pub fn search_candidates(query: &str, ecosystem: &str, total_limit: usize) -> Vec<Candidate> {
    let key = format!(
        "{}|{}|{}",
        query.trim().to_lowercase(),
        ecosystem.to_lowercase(),
        total_limit
    );
    if let Some(hit) = cache_get(&key) {
        return hit;
    }

    let results = match find_source(ecosystem) {
        Some(search) => run_logged(search, query, total_limit),
        None if ecosystem.eq_ignore_ascii_case("all") => fan_out_all(query, total_limit),
        None => {
            eprintln!("Warning: unknown ecosystem '{}'", ecosystem);
            Vec::new()
        }
    };

    cache_put(&key, &results);
    results
}

/// Query every source in parallel; results joined in table order.
fn fan_out_all(query: &str, total_limit: usize) -> Vec<Candidate> {
    let per_source = total_limit.div_ceil(SOURCES.len()).max(2);
    let handles: Vec<_> = SOURCES
        .iter()
        .map(|&(_, _, search)| {
            let q = query.to_string();
            std::thread::spawn(move || run_logged(search, &q, per_source))
        })
        .collect();
    handles
        .into_iter()
        .flat_map(|h| h.join().unwrap_or_default())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_specials() {
        assert_eq!(encode_query("rust grep"), "rust+grep");
        assert_eq!(encode_query("a&b"), "a%26b");
        assert_eq!(encode_query("c#"), "c%23");
        assert_eq!(encode_query("tokio-rs"), "tokio-rs");
    }

    #[test]
    fn parses_duckduckgo_fixture() {
        let good = r#"" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=x">Example Page</a>"#;
        let c = parse_ddg_block(good).expect("fixture must parse");
        assert_eq!(c.name, "Example Page");
        assert!(c.url.contains("example.com"));
        assert_eq!(c.ecosystem, "web");
        assert!(parse_ddg_block("no link here").is_none());
        assert!(parse_ddg_block(r#"" href="/relative/path">T</a>"#).is_none());
    }

    fn terms(q: &str) -> Vec<String> {
        query_terms(q)
    }

    #[test]
    fn resolves_ecosystem_aliases() {
        assert!(find_source("nixos").is_some());
        assert!(find_source("MELPA").is_some());
        assert!(find_source("crates.io").is_some());
        assert!(find_source("pypi").is_none());
        assert!(is_valid_ecosystem("all"));
        assert!(!is_valid_ecosystem("bogus"));
        assert_eq!(ecosystem_names().count(), SOURCES.len());
    }

    #[test]
    fn splits_query_terms() {
        assert_eq!(terms("Git porcelain, for Emacs!"), ["git", "porcelain", "for", "emacs"]);
        assert!(terms("  --  ").is_empty());
    }

    #[test]
    fn converts_melpa_snapshot_dates() {
        assert_eq!(melpa_date(&[20190703, 527]), "2019-07-03T00:00:00Z");
        assert_eq!(melpa_date(&[1, 2, 3]), "");
        assert_eq!(melpa_date(&[]), "");
    }

    #[test]
    fn builds_melpa_candidates_from_fixture() {
        let fixture = r#"{
            "magit": {"ver":[20260920,1200],"deps":null,"desc":"A Git porcelain inside Emacs","type":"tar",
                      "props":{"url":"https://github.com/magit/magit","keywords":["git","tools","vc"]}},
            "bare":  {"ver":[20200101,1],"deps":null,"desc":"","type":"single"}
        }"#;
        let packages: HashMap<String, MelpaEntry> = serde_json::from_str(fixture).unwrap();
        let magit = melpa_candidate("magit", &packages["magit"], 42);
        assert_eq!(magit.id, "melpa:magit");
        assert_eq!(magit.url, "https://github.com/magit/magit");
        assert_eq!(magit.updated_at, "2026-09-20T00:00:00Z");
        assert_eq!(magit.topics, ["git", "tools", "vc"]);
        assert_eq!(magit.downloads, 42);
        let bare = melpa_candidate("bare", &packages["bare"], 0);
        assert_eq!(bare.url, "https://melpa.org/#/bare");
        assert_eq!(bare.description, "No description provided");
    }

    #[test]
    fn melpa_score_prefers_name_over_description() {
        let t = terms("git porcelain");
        let kw = |k: &[&str]| k.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let name_hit = melpa_match_score(&t, "git-porcelain", "Some tool", &[]);
        let desc_hit = melpa_match_score(&t, "magit", "A Git porcelain inside Emacs", &kw(&["vc"]));
        let miss = melpa_match_score(&t, "org-roam", "Roam replica with Org-mode", &kw(&["org"]));
        assert!(desc_hit > 0, "description match must count");
        assert!(name_hit > desc_hit, "name match must outrank description-only match");
        assert_eq!(miss, 0);
        // Stopwords alone never match; covering every word beats one strong name hit.
        assert_eq!(melpa_match_score(&terms("emacs mode"), "emacs-mode", "Emacs mode", &[]), 0);
        let both = melpa_match_score(&terms("markdown preview"), "md", "Markdown live preview", &[]);
        let one = melpa_match_score(&terms("markdown preview"), "markdown-x", "Unrelated", &[]);
        assert!(both > one, "coverage {} must beat single name hit {}", both, one);
    }

    #[test]
    fn picks_highest_nix_alias_for_channel() {
        let cat = "latest-51-nixos-unstable\nlatest-45-nixos-unstable\nlatest-9-nixos-unstable\n\
                   latest-52-nixos-26.05\n.kibana\nlatest-51-group-manual\n";
        assert_eq!(pick_nix_alias(cat, "unstable").as_deref(), Some("latest-51-nixos-unstable"));
        assert_eq!(pick_nix_alias(cat, "26.05").as_deref(), Some("latest-52-nixos-26.05"));
        assert_eq!(pick_nix_alias(cat, "bogus"), None);
    }

    #[test]
    fn parses_nix_hits_fixture() {
        let json: serde_json::Value = serde_json::from_str(r#"{"hits":{"hits":[
            {"_source":{"package_attr_name":"sqlit-tui","package_pversion":"1.4.0",
                        "package_description":"TUI for SQL","package_homepage":["https://github.com/Maxteabag/sqlit"],
                        "package_license_set":["MIT License","Apache License 2.0"]}},
            {"_source":{"package_attr_name":"nohome","package_homepage":[],"package_license_set":[]}},
            {"_source":{"package_attr_name":""}}
        ]}}"#).unwrap();
        let hits = parse_nix_hits(&json, 10).unwrap();
        assert_eq!(hits.len(), 2, "empty attr name is dropped");
        assert_eq!(hits[0].install_cmd, "nix shell nixpkgs#sqlit-tui");
        assert_eq!(hits[0].license, "MIT License, Apache License 2.0");
        assert_eq!(hits[0].topics, ["1.4.0"]);
        assert_eq!(hits[1].url, "https://search.nixos.org/packages?query=nohome");
        assert_eq!(hits[1].license, "Unknown");
        assert!(parse_nix_hits(&serde_json::json!({}), 5).is_err());
    }
}
