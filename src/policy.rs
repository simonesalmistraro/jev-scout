//! One place for every threshold. Tune here, nowhere else.

/// Minimum fit score (0-3 scale, probability-weighted level position) to survive.
pub const MIN_FIT: f64 = 1.5;
/// Minimum answer confidence to survive filtering.
pub const MIN_CONFIDENCE: f64 = 0.5;
/// Stale penalty: candidates untouched this long lose 10% rank weight.
pub const STALE_DAYS: i64 = 180;
pub const STALE_PENALTY: f64 = 0.9;
/// Orphan penalty: registry reports zero maintainers (nixpkgs). Compounds with stale.
pub const ORPHAN_PENALTY: f64 = 0.8;

/// Composite fit dimensions and code-owned weights. Sums to 1.0.
/// Maturity rides the deterministic stale penalty, not a question.
/// MELPA local ranking (no upstream search API). Points per query word by where it hits:
/// a whole name segment (`git` in `git-gutter`), inside the name, an exact keyword,
/// or a description word. Each matched word adds a coverage bonus so multi-word
/// matches beat one strong hit.
pub const MELPA_NAME_SEGMENT: u32 = 6;
pub const MELPA_NAME_SUBSTRING: u32 = 4;
pub const MELPA_KEYWORD: u32 = 3;
pub const MELPA_DESCRIPTION: u32 = 1;
pub const MELPA_COVERAGE_BONUS: u32 = 5;
/// MELPA snapshot rebuilds run every few hours; the on-disk archive copy is reused this long.
pub const MELPA_DISK_TTL_SECS: u64 = 6 * 60 * 60;
/// Words nearly every MELPA package matches; they carry no signal.
pub const MELPA_STOPWORDS: &[&str] = &[
    "a", "an", "and", "the", "for", "in", "of", "on", "to", "with", "that", "is", "my", "way",
    "emacs", "elisp", "el", "mode", "package", "plugin", "tool",
];

pub const FIT_WEIGHTS: &[(&str, f64)] = &[
    ("fit", 0.7),
    ("doc", 0.3),
];
