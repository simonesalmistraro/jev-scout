# Lessons

- 2026-09-23: Do not leave TODO(user) stubs for the user to implement (learning-mode habit). Implement fully, put tunables in `src/policy.rs`.
- 2026-09-23: `cargo` is only on PATH inside `nix develop`; `cargo test | tail` in a `&&` chain hides "command not found". Run gates via `nix develop -c`.
