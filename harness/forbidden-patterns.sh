# Project-specific forbidden patterns for redis-rs-port.
# Sourced by port-harness/hooks/forbidden-pattern.sh.
#
# Redis differs from Lua in what's banned:
#   - Redis IS network code. Async (tokio/futures) is allowed. Lua wasn't.
#   - Redis strings are bytes (keys, values, RESP payloads). Banning
#     `from_utf8` on RESP data is essential; in Lua it was banning
#     String/&str for Lua data.

FORBIDDEN_PATTERNS=(
    'std::str::from_utf8|String::from_utf8|from_utf8_unchecked'
)

PATH_EXCEPTIONS=(
    ''
)

# Future additions (uncomment + add a parallel exception once we have
# concrete cases that need them):
#
# - 'panic!\(' with no exception (test-fixer should propose Err, not panic).
# - 'unwrap\(\)' outside test code (same).
# - 'std::process::Command' outside redis-cli (if/when we add that crate).
