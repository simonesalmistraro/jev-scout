# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview
`jev-scout` is a sub-second CLI and stdio MCP server (Rust 2021) that finds real, actively-maintained GitHub repos, crates.io crates, and web results matching a natural-language prompt. It grounds discovery in live registries first, then scores candidate fit with TypeSafe AI's Jev decision model — so it never hallucinates package names.

## Build / Test / Lint
```bash
cargo build --release
cargo test                       # all offline mock tests, zero token/network spend
cargo test stale_boundary        # single test by name (substring match)
cargo test --test mock_test      # only the integration test file
cargo clippy -- -D warnings      # lint gate (CI treats warnings as errors)
cargo fmt --check                # format gate
```
Tests are pure offline: `tests/mock_test.rs` redeclares local structs and never hits the network. `jev.rs` has inline unit tests (`stale_boundary`, `weak_filter_thresholds`). CI (`.github/workflows/ci.yml`) runs `cargo check --all-targets` + `cargo test --all`.

## Running
Requires `TYPESAFE_API_KEY` (get from https://typesafe.ai). GitHub search uses `GITHUB_TOKEN`/`GH_TOKEN` or falls back to `gh auth token` when present (raises rate limit).
```bash
TYPESAFE_API_KEY=... cargo run -- "fast sqlite tui in rust" --ecosystem rust --json
TYPESAFE_API_KEY=... cargo run -- --mcp        # stdio JSON-RPC 2.0 MCP server
```
Ecosystems: `all` (default), `github`, `crates`, `web`. `--limit` clamps to 1..10.

## Pipeline (the big picture)
`main.rs` orchestrates three deterministic stages; the LLM only judges, never computes:
1. **Ground** — `search::search_candidates` fans out across GitHub REST + crates.io (+ web) in parallel threads, returns up to 8 real `Candidate`s.
2. **Score** — `jev::evaluate_candidates` sends candidates to `POST https://api.typesafe.ai/v1/systemone`. Per candidate it asks a `score` (fit), a `score` (docs), and a `noul` (actively maintained); plus one `choice` (single best_match) across all. **Candidates are chunked by 3 and fanned out per chunk** — Jev silently drops questions past ~15/call, so one big request would lose answers.
3. **Rank + filter** — host code composites and sorts by `weighted_rank`; `filter_weak` drops candidates under policy floors unless `--no-filter`.

## Module map
| File | Role |
|------|------|
| `main.rs` | `lexopt` arg parsing, orchestration, ANSI terminal cards, `--json` |
| `search.rs` | Candidate retrieval (GitHub/crates.io/web), token discovery, 60s TTL cache |
| `jev.rs` | Jev request build, chunked fan-out, answer parsing, ranking, `filter_weak`, stale math, 60s eval cache |
| `policy.rs` | **Single source of truth for every threshold/weight.** Tune here, nowhere else |
| `trend.rs` | Remembers top pick per query in `~/.jev-scout/history.json` (std-only), reports movement |
| `types.rs` | `Candidate`, `EvaluatedCandidate`, Jev request/response structs |
| `mcp.rs` | Stdio JSON-RPC 2.0 loop, protocol negotiation, exposes `scout_repos` tool |

## Load-bearing constraints (violating these breaks the design)
- **Zero hallucination**: never emit repo/crate names from memory — always search a registry, then score real candidates.
- **LLM judges, host computes**: all date/staleness/ranking math lives in host code. `is_stale_180d` uses Julian-day arithmetic (no `chrono`) *on purpose* — dates must never go to Jev.
- **Composite fit** = `fit*0.7 + doc*0.3` on a **0–3 scale** (see `FIT_WEIGHTS`); modern% is the `noul` value; best_match is the `choice`. The old "1–4 fit / single fan-out" description is stale — trust the code.
- **Fail loud, never default**: a malformed/missing Jev answer skips that candidate with a `Warning:` to stderr — it never silently substitutes a default score. `confidence` is the `min` across a candidate's dimensions.
- **Two in-process caches** (`search.rs`, `jev.rs`), both 60s TTL, whole-map sweep (no LRU). They make warm MCP-session repeats ~0.3s. Keyed on normalized query + sorted candidate ids.
- **Strict budget**: `ureq` blocking (no async runtime), `lexopt` (no clap), no new deps without cause, 8s per-upstream timeout, single static binary, $0 paid APIs.
- **Model is pinned** to a version string in `jev.rs` (currently `jev-1.13.0`), not `jev-latest`.

## Commit style (from AGENTS.md)
Imperative conventional commits (`feat:`/`fix:`/`docs:`/`chore:`/`perf:`), subject <72 chars, **no em dashes** in commits or docs.
