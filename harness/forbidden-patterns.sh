# Project-specific forbidden patterns for redis-rs-port.
# Sourced by port-harness/hooks/forbidden-pattern.sh.
#
# Redis differs from Lua in what's banned:
#   - Redis IS network code. Async (tokio/futures) is allowed. Lua wasn't.
#   - Redis strings are bytes (keys, values, RESP payloads). Banning
#     `from_utf8` on RESP data is essential; in Lua it was banning
#     String/&str for Lua data.
#
# Patterns target the DANGEROUS uses (assumes UTF-8 valid → loses round-
# trip fidelity), not all uses. Safe match/Err-branch patterns like
# `match from_utf8(b) { Ok(s) => ..., Err(_) => ... }` in Debug impls
# are fine — they handle non-UTF8 explicitly.

FORBIDDEN_PATTERNS=(
    # from_utf8_unchecked — undefined behavior on non-UTF8 bytes
    '\bfrom_utf8_unchecked\b'
    # from_utf8(...).unwrap() / .expect() — assumes UTF-8, panics on real Redis data
    'from_utf8[[:space:]]*\([^)]*\)[[:space:]]*\.[[:space:]]*unwrap'
    'from_utf8[[:space:]]*\([^)]*\)[[:space:]]*\.[[:space:]]*expect'
    # String::from_utf8 chained with unwrap — same hazard
    'String::from_utf8[[:space:]]*\([^)]*\)[[:space:]]*\.[[:space:]]*unwrap'
)

PATH_EXCEPTIONS=(
    ''
    ''
    ''
    ''
)

# Future additions (uncomment + add a parallel exception once we have
# concrete cases that need them):
#
# - 'panic!\(' with no exception (test-fixer should propose Err, not panic).
# - 'unwrap\(\)' outside test code (same).
# - 'std::process::Command' outside redis-cli (if/when we add that crate).
