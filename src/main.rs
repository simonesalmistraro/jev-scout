mod jev;
mod mcp;
mod policy;
mod search;
mod trend;
mod types;

use lexopt::prelude::*;
use std::process;
use std::time::Instant;

fn print_help() {
    let help = format!(
        r#"jev-scout {} - Zero-hallucination open-source repo and crate scout

USAGE:
    jev-scout [OPTIONS] <QUERY>

ARGS:
    <QUERY>                 Natural language description of what you are searching for

OPTIONS:
    -e, --ecosystem <NAME>  Target ecosystem: 'all' (default), {}
    -n, --limit <NUM>       Maximum results to show (default: 5)
    -j, --json              Output machine-readable JSON to stdout
        --no-filter         Show all candidates, skip weak-match filtering
        --mcp               Run as a stdio Model Context Protocol (MCP) server
    -h, --help              Print help information
    -v, --version           Print version information

EXAMPLES:
    jev-scout "fast sqlite tui in rust"
    jev-scout "headless browser without chromium" --ecosystem rust
    jev-scout "token efficient grep for coding agents" --json
    jev-scout "git porcelain" --ecosystem emacs
    jev-scout "sqlite tui" --ecosystem nix      # JEV_NIX_CHANNEL=26.05 to pin a release
"#,
        env!("CARGO_PKG_VERSION"),
        search::ecosystem_names()
            .map(|e| format!("'{}'", e))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("{}", help);
}

fn main() {
    let mut query: Option<String> = None;
    let mut ecosystem = "all".to_string();
    let mut limit = 5usize;
    let mut json_mode = false;
    let mut no_filter = false;
    let mut mcp_mode = false;

    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next().unwrap_or_else(|e| {
        eprintln!("Error: {}", e);
        process::exit(1);
    }) {
        match arg {
            Short('e') | Long("ecosystem") => {
                ecosystem = parser
                    .value()
                    .ok()
                    .and_then(|v| v.string().ok())
                    .unwrap_or_else(|| "all".to_string());
            }
            Short('n') | Long("limit") => {
                limit = parser
                    .value()
                    .ok()
                    .and_then(|v| v.string().ok())
                    .and_then(|s| s.parse::<usize>().ok())
                    .map(|n| n.clamp(1, 10))
                    .unwrap_or(5);
            }
            Short('j') | Long("json") => {
                json_mode = true;
            }
            Long("no-filter") => {
                no_filter = true;
            }
            Long("mcp") => {
                mcp_mode = true;
            }
            Short('h') | Long("help") => {
                print_help();
                process::exit(0);
            }
            Short('v') | Long("version") => {
                println!("jev-scout {}", env!("CARGO_PKG_VERSION"));
                process::exit(0);
            }
            Value(val) => {
                query = Some(val.string().unwrap_or_default());
            }
            _ => {
                eprintln!("Error: unexpected argument {:?}", arg);
                print_help();
                process::exit(1);
            }
        }
    }

    let api_key = match jev::resolve_api_key() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("Error: {}", e);
            process::exit(1);
        }
    };

    if mcp_mode {
        if let Err(e) = mcp::run_mcp_server(&api_key) {
            eprintln!("MCP Server Error: {}", e);
            process::exit(1);
        }
        return;
    }

    let query_str = match query {
        Some(q) if !q.trim().is_empty() => q,
        _ => {
            print_help();
            process::exit(1);
        }
    };

    if !search::is_valid_ecosystem(&ecosystem) {
        eprintln!(
            "Error: unknown ecosystem '{}'. Use all, {}.",
            ecosystem,
            search::ecosystem_names().collect::<Vec<_>>().join(", ")
        );
        process::exit(2);
    }

    let start_time = Instant::now();

    if !json_mode {
        println!("🔍 Scouting repositories for: \"{}\"", query_str);
    }

    let search_start = Instant::now();
    let candidates = search::search_candidates(&query_str, &ecosystem, 8);
    let search_ms = search_start.elapsed().as_millis();
    if candidates.is_empty() {
        if json_mode {
            println!("[]");
        } else {
            println!("No candidates found matching query.");
        }
        return;
    }

    let eval_start = Instant::now();
    let evaluated = match jev::evaluate_candidates(&query_str, candidates, &api_key) {
        Ok(res) => res,
        Err(err) => {
            eprintln!("Error: {}", err);
            process::exit(1);
        }
    };
    let eval_ms = eval_start.elapsed().as_millis();
    let elapsed = start_time.elapsed();

    let scored_count = evaluated.len();
    let top_results: Vec<_> = if no_filter {
        evaluated.into_iter().take(limit).collect()
    } else {
        jev::filter_weak(evaluated)
            .into_iter()
            .take(limit)
            .collect()
    };
    if top_results.is_empty() && scored_count > 0 {
        eprintln!(
            "All {} candidates scored below fit {} or confidence {}. Rerun with --no-filter to see them.",
            scored_count,
            policy::MIN_FIT,
            policy::MIN_CONFIDENCE
        );
    }

    if json_mode {
        println!("{}", serde_json::to_string_pretty(&top_results).unwrap());
        return;
    }

    println!(
        "Found {} candidates evaluated in {:.0}ms via TypeSafe Jev (search {:.0}ms + eval {:.0}ms):\n",
        top_results.len(),
        elapsed.as_millis(),
        search_ms,
        eval_ms
    );

    if let Some(first) = top_results.first() {
        match trend::record(&query_str, &first.candidate.id) {
            Some(prev) => println!("  Previously topped by: {}\n", prev),
            None => println!("  First recorded top pick for this query.\n"),
        }
    }

    for (rank, item) in top_results.iter().enumerate() {
        let best_tag = if item.is_best_match {
            " \x1b[32;1m[BEST MATCH]\x1b[0m"
        } else {
            ""
        };

        println!(
            "\x1b[1m#{}\x1b[0m  \x1b[36;1m{}\x1b[0m{}",
            rank + 1,
            item.candidate.name,
            best_tag
        );
        println!(
            "    {} | \x1b[35m{}\x1b[0m | Updated: {}",
            popularity_label(&item.candidate),
            item.candidate.license,
            item.candidate.updated_at.get(..10).unwrap_or("n/a")
        );
        if !item.candidate.topics.is_empty() {
            println!(
                "    \x1b[90m{}\x1b[0m",
                item.candidate
                    .topics
                    .iter()
                    .map(|t| format!("#{}", t))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        println!("    {}", item.candidate.description);
        println!(
            "    Fit: \x1b[32m{:.1}/3.0\x1b[0m (Conf: {:.2}) | Active: \x1b[34m{:.0}%\x1b[0m",
            item.fit_score,
            item.confidence,
            item.is_modern * 100.0
        );
        println!("    URL: \x1b[4m{}\x1b[0m", item.candidate.url);
        println!(
            "    \x1b[90mCommand:\x1b[0m {}\n",
            item.candidate.install_cmd
        );
    }
}

fn format_num(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Registry-appropriate popularity signal for the card header.
fn popularity_label(c: &types::Candidate) -> String {
    match c.ecosystem.as_str() {
        "crates.io" | "melpa" => format!("⬇ {}", format_num(c.downloads)),
        "nixpkgs" => match c.maintainers {
            Some(0) => format!("📦 {} distros | orphaned", c.stars),
            Some(1) => format!("📦 {} distros | 1 maintainer", c.stars),
            Some(n) => format!("📦 {} distros | {} maintainers", c.stars, n),
            None => format!("📦 {} distros", c.stars),
        },
        "web" => "web".to_string(),
        _ => format!("⭐ {}", format_num(c.stars)),
    }
}
