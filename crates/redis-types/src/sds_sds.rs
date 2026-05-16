//! Port of `sds.c` (1314 lines, ~54 functions) and `sds.h` (300 lines,
//! inline functions + type definitions).
//!
//! # Architecture note
//!
//! In C, the SDS (Simple Dynamic Strings) library stores the string length and
//! allocated capacity in a small typed header immediately before the data
//! pointer. Five header types (`sdshdr5` through `sdshdr64`) minimise overhead
//! for strings of different lengths. All of that bookkeeping is transparent in
//! Rust: `Vec<u8>` already tracks `len` and `capacity`, and the global
//! allocator handles growth. Therefore the memory-layout machinery — header
//! types, `sdsHdrSize`, `sdsReqType`, `sdswrite`, `sdsAllocPtr`,
//! `sdsIncrLen`, `sds_malloc/realloc/free` — has **no Rust translation** and
//! is noted inline below.
//!
//! This module provides the **semantic operations** from the SDS public API
//! as free functions operating on `&mut RedisString` / `&[u8]`.
//!
//! # What is NOT ported (and why)
//!
//! | C symbol | Reason skipped |
//! |---|---|
//! | `sdshdr5..64`, `SDS_TYPE_*`, `SDS_HDR_VAR` | C-level tagged-header layout; `Vec<u8>` handles this |
//! | `sdsHdrSize`, `sdsReqType`, `sdsTypeMaxSize` | Header sizing; Vec is self-managing |
//! | `sdsType`, `sdsavail`, `sdsalloc`, `sdssetlen`, `sdsinclen`, `sdssetalloc` | Inline header accessors; all internal to Vec |
//! | `sdsGetAuxBit`, `sdsSetAuxBit` | Aux bits packed in unused flag bits; no Rust equivalent needed |
//! | `sdswrite`, `adjustTypeIfNeeded` | Pointer arithmetic into typed headers; purely internal C allocation detail |
//! | `_sdsnewlen`, `sdstrynewlen` | Try-alloc is not stable Rust; OOM handling is separate |
//! | `sdsupdatelen` | Rescans for NUL to fix length after manual C buffer surgery; not needed in safe Rust |
//! | `_sdsMakeRoomFor` | `Vec::reserve` / `Vec::reserve_exact` are the direct equivalents |
//! | `sdsAllocSize`, `sdsAllocPtr` | Allocation metadata; use `Vec::capacity` / pointer not needed in safe Rust |
//! | `sdsIncrLen` | Used after manual C buffer writes; safe Rust uses push/extend instead |
//! | `sdsfree`, `sdsfreeVoid` | `Drop` handles this |
//! | `sds_malloc`, `sds_realloc`, `sds_free` | Allocator wrappers; Rust global allocator |
//! | `SDS_NOINIT` | C sentinel for "skip memset"; in Rust use `Vec::with_capacity` + extend |
//!
//! C: sds.c (1314 lines, 54 functions), sds.h (300 lines)

use crate::error::RedisError;
use crate::string::RedisString;

/// Maximum pre-allocation target for greedy-grow operations.
/// C: `SDS_MAX_PREALLOC = 1024 * 1024` in `sds.h`
pub const SDS_MAX_PREALLOC: usize = 1024 * 1024;

// ──────────────────────────────────────────────────────────────────────────
// Factory functions
// ──────────────────────────────────────────────────────────────────────────

/// Creates a new `RedisString` initialised from the given byte slice.
/// C: `sdsnewlen` / `sdsnew` (sds.c:168-186)
pub fn new_from_bytes(init: &[u8]) -> RedisString {
    RedisString::from_bytes(init)
}

/// Creates an empty `RedisString`.
/// C: `sdsempty` (sds.c:178-180)
pub fn new_empty() -> RedisString {
    RedisString::new()
}

/// Creates a `RedisString` containing the ASCII decimal representation of
/// an `i64` value.
/// C: `sdsfromlonglong` (sds.c:568-573) — calls `ll2string` from `util.c`.
pub fn from_long_long(value: i64) -> RedisString {
    RedisString::from_vec(itoa_i64(value))
}

/// Converts an `i64` to its ASCII decimal byte representation.
/// C: `ll2string` in `util.c` (called by `sdsfromlonglong`).
///
/// PORT NOTE: `format!("{}", n).into_bytes()` would work too, but that routes
/// through Rust's UTF-8 `String`. Since the output is always pure ASCII digits
/// (and an optional leading `'-'`), the manual implementation avoids any UTF-8
/// allocation and is unambiguously byte-correct.
fn itoa_i64(mut n: i64) -> Vec<u8> {
    if n == i64::MIN {
        return b"-9223372036854775808".to_vec();
    }
    if n == 0 {
        return vec![b'0'];
    }
    let negative = n < 0;
    if negative {
        n = -n;
    }
    let mut buf = Vec::with_capacity(20);
    while n > 0 {
        buf.push(b'0' + (n % 10) as u8);
        n /= 10;
    }
    if negative {
        buf.push(b'-');
    }
    buf.reverse();
    buf
}

// ──────────────────────────────────────────────────────────────────────────
// Capacity management helpers
// ──────────────────────────────────────────────────────────────────────────

/// Grows the string to exactly `len` bytes, zero-filling any newly added bytes.
/// If `len <= s.len()`, this is a no-op (never shrinks).
/// C: `sdsgrowzero` (sds.c:500-511)
pub fn grow_zero(s: &mut RedisString, len: usize) {
    let curlen = s.len();
    if len <= curlen {
        return;
    }
    s.extend_from_slice(&vec![0u8; len - curlen]);
}

/// Signals that the caller intends to append `addlen` additional bytes, so the
/// underlying buffer should be pre-grown using the C greedy-doubling policy:
/// double below `SDS_MAX_PREALLOC`, add `SDS_MAX_PREALLOC` above.
///
/// C: `sdsMakeRoomFor` → `_sdsMakeRoomFor(s, addlen, greedy=1)` (sds.c:310-312)
///
/// PORT NOTE: `Vec::reserve` uses the allocator's own growth policy, which may
/// differ from the C doubling policy. The semantic contract (at least `addlen`
/// additional bytes become available) is preserved; the exact over-allocation
/// is not.
///
/// TODO(architect): `RedisString` does not yet expose a `reserve(n)` method.
/// Until it does, callers should rely on `Vec::reserve` via `extend_from_slice`,
/// which triggers growth implicitly. This stub documents intent only.
pub fn make_room_for(_s: &mut RedisString, _addlen: usize) {
    // TODO(port): RedisString::reserve() not yet available. Callers that need
    // guaranteed pre-allocation must work through the Vec directly (Phase B).
}

/// Releases excess capacity so that allocated size equals used length.
/// C: `sdsRemoveFreeSpace(s, 0)` (sds.c:325-327)
///
/// TODO(architect): `RedisString` needs a `shrink_to_fit()` wrapper for this.
/// PERF(port): C `sdsRemoveFreeSpace` reallocates to exactly
/// `sdslen(s) + hdrlen + 1`. `Vec::shrink_to_fit` may over-shoot on some
/// allocators (e.g. jemalloc size-classes).
pub fn shrink_to_fit(_s: &mut RedisString) {
    // TODO(port): RedisString::shrink_to_fit() not yet available (Phase B).
}

// ──────────────────────────────────────────────────────────────────────────
// Append / copy operations
// ──────────────────────────────────────────────────────────────────────────

/// Appends a byte slice to the string, growing as needed.
/// C: `sdscatlen` (sds.c:518-527), `sdscat` (sds.c:533-535),
///    `sdscatsds` (sds.c:541-543)
pub fn cat_bytes(s: &mut RedisString, t: &[u8]) {
    s.extend_from_slice(t);
}

/// Replaces the entire content of the string with the given bytes.
/// C: `sdscpylen` (sds.c:547-556), `sdscpy` (sds.c:560-562)
pub fn copy_bytes(s: &mut RedisString, t: &[u8]) {
    s.clear();
    s.extend_from_slice(t);
}

/// Appends a pre-formatted byte slice to the string.
///
/// C: `sdscatvprintf` / `sdscatprintf` (sds.c:576-640).
///
/// PORT NOTE: In C, `sdscatprintf` is variadic (`const char *fmt, ...`).
/// Rust has no stable variadic functions. Callers should use `write!` or
/// `format!` and pass the result here, e.g.:
/// ```ignore
/// let msg = format!("value is {}", x).into_bytes();
/// cat_formatted(&mut s, &msg);
/// ```
pub fn cat_formatted(s: &mut RedisString, formatted: &[u8]) {
    s.extend_from_slice(formatted);
}

// ──────────────────────────────────────────────────────────────────────────
// Custom fast format — sdscatfmt
// ──────────────────────────────────────────────────────────────────────────
//
// C: `sdscatfmt` (sds.c:658-751) supports format specifiers:
//   %s (C string), %S (SDS string), %i (int), %I (i64), %u (uint), %U (u64), %%
//
// TODO(port): sdscatfmt uses C varargs (`...`). There is no stable variadic
// function mechanism in Rust. Phase B callers should use the standard
// `write!` / `format!` macros instead. If the custom formatter's
// performance advantage proves necessary (it avoids libc printf overhead),
// it can be provided as a Rust macro in Phase B.

// ──────────────────────────────────────────────────────────────────────────
// String manipulation — in-place
// ──────────────────────────────────────────────────────────────────────────

/// Trims leading and trailing bytes from the string: any byte present in
/// `cset` is removed from the front and back. The middle is unchanged.
/// C: `sdstrim` (sds.c:767-780)
pub fn trim(s: &mut RedisString, cset: &[u8]) {
    let bytes = s.as_bytes();
    let start = bytes
        .iter()
        .position(|b| !cset.contains(b))
        .unwrap_or(bytes.len());
    let result: Vec<u8> = match bytes.iter().rposition(|b| !cset.contains(b)) {
        None => Vec::new(),
        Some(end) if start > end => Vec::new(),
        Some(end) => bytes[start..=end].to_vec(),
    };
    copy_bytes(s, &result);
}

/// Replaces the string content with the `len`-byte substring starting at
/// byte index `start`. Clamps both arguments to valid ranges.
/// C: `sdssubstr` (sds.c:785-795)
pub fn substr(s: &mut RedisString, start: usize, len: usize) {
    let oldlen = s.len();
    let real_start = start.min(oldlen);
    let real_len = len.min(oldlen.saturating_sub(real_start));
    let result: Vec<u8> = s.as_bytes()[real_start..real_start + real_len].to_vec();
    copy_bytes(s, &result);
}

/// In-place range with Python / Redis-style negative-index support (inclusive
/// on both ends). `-1` refers to the last byte, `-2` to the penultimate, etc.
///
/// If `start > end` after resolution, the result is empty.
///
/// NOTE: following the C semantics, `start == end` yields a one-byte string.
/// Use `substr` when you want a zero-length result from an empty range.
/// C: `sdsrange` (sds.c:818-825)
pub fn range(s: &mut RedisString, mut start: isize, mut end: isize) {
    if s.is_empty() {
        return;
    }
    let len = s.len() as isize;
    if start < 0 {
        start += len;
    }
    if end < 0 {
        end += len;
    }
    let newlen = if start > end { 0 } else { (end - start + 1) as usize };
    let real_start = start.max(0) as usize;
    substr(s, real_start, newlen);
}

/// Converts every ASCII byte to its lowercase equivalent in-place.
/// Non-ASCII bytes are left unchanged.
/// C: `sdstolower` (sds.c:828-832)
pub fn to_lower(s: &mut RedisString) {
    let lowered: Vec<u8> = s.as_bytes().iter().map(|b| b.to_ascii_lowercase()).collect();
    copy_bytes(s, &lowered);
}

/// Converts every ASCII byte to its uppercase equivalent in-place.
/// Non-ASCII bytes are left unchanged.
/// C: `sdstoupper` (sds.c:835-839)
pub fn to_upper(s: &mut RedisString) {
    let uppered: Vec<u8> = s.as_bytes().iter().map(|b| b.to_ascii_uppercase()).collect();
    copy_bytes(s, &uppered);
}

/// Sets the string length to zero without releasing the underlying allocation.
/// C: `sdsclear` (sds.c:229-232)
pub fn clear(s: &mut RedisString) {
    s.clear();
}

/// Compares two `RedisString`s bytewise; the longer one is greater when the
/// shorter is a prefix of it.
///
/// PORT NOTE: `RedisString` implements `Ord` via lexicographic byte comparison,
/// which is identical to this logic. Use `s1.cmp(s2)` directly.
/// This function exists for explicit API parity with `sdscmp`.
/// C: `sdscmp` (sds.c:852-862)
pub fn cmp(s1: &RedisString, s2: &RedisString) -> std::cmp::Ordering {
    s1.cmp(s2)
}

/// Maps characters in-place: each byte present in `from` is replaced by the
/// byte at the same index in `to`. Bytes not in `from` are unchanged.
/// C: `sdsmapchars` (sds.c:1205-1217)
pub fn map_chars(s: &mut RedisString, from: &[u8], to: &[u8]) {
    let mapped: Vec<u8> = s
        .as_bytes()
        .iter()
        .map(|&b| match from.iter().position(|&f| f == b) {
            Some(idx) => to.get(idx).copied().unwrap_or(b),
            None => b,
        })
        .collect();
    copy_bytes(s, &mapped);
}

// ──────────────────────────────────────────────────────────────────────────
// Printable / escaped representation
// ──────────────────────────────────────────────────────────────────────────

/// Appends a printable, escaped representation of `p` to `s`, enclosed in
/// double quotes. Non-printable bytes are emitted as `\n`, `\r`, `\t`, `\a`,
/// `\b`, or `\xNN`. Backslash and double-quote are escaped as `\\` and `\"`.
/// C: `sdscatrepr` (sds.c:940-972)
pub fn cat_repr(s: &mut RedisString, p: &[u8]) {
    s.push(b'"');
    let mut pos = 0;
    while pos < p.len() {
        let b = p[pos];
        let is_plain = b >= 0x20 && b <= 0x7e && b != b'\\' && b != b'"';
        if is_plain {
            let run_start = pos;
            while pos < p.len() {
                let c = p[pos];
                if !(c >= 0x20 && c <= 0x7e && c != b'\\' && c != b'"') {
                    break;
                }
                pos += 1;
            }
            s.extend_from_slice(&p[run_start..pos]);
        } else {
            match b {
                b'\\' | b'"' => {
                    s.push(b'\\');
                    s.push(b);
                }
                b'\n' => s.extend_from_slice(b"\\n"),
                b'\r' => s.extend_from_slice(b"\\r"),
                b'\t' => s.extend_from_slice(b"\\t"),
                0x07 => s.extend_from_slice(b"\\a"),
                0x08 => s.extend_from_slice(b"\\b"),
                c => {
                    let hex = b"0123456789abcdef";
                    s.push(b'\\');
                    s.push(b'x');
                    s.push(hex[((c >> 4) & 0x0f) as usize]);
                    s.push(hex[(c & 0x0f) as usize]);
                }
            }
            pos += 1;
        }
    }
    s.push(b'"');
}

/// Returns `true` if the string contains any byte that `cat_repr` would
/// escape: non-printable bytes, spaces, backslash, or double-quote.
/// C: `sdsneedsrepr` (sds.c:981-993)
pub fn needs_repr(s: &RedisString) -> bool {
    for &b in s.as_bytes() {
        let printable = b >= 0x20 && b <= 0x7e;
        let is_space = matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
        if b == b'\\'
            || b == b'"'
            || b == b'\n'
            || b == b'\r'
            || b == b'\t'
            || b == 0x07
            || b == 0x08
            || !printable
            || is_space
        {
            return true;
        }
    }
    false
}

/// Returns `true` if `c` is a valid ASCII hexadecimal digit.
/// C: `is_hex_digit` (sds.c:997-999)
fn is_hex_digit(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

/// Converts a single ASCII hex digit to its integer value (0–15).
/// Returns 0 for any non-hex byte (matching C fall-through default).
/// C: `hex_digit_to_int` (sds.c:1003-1029)
fn hex_digit_to_int(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Split by fixed separator
// ──────────────────────────────────────────────────────────────────────────

/// Splits `s` at every occurrence of the binary separator `sep`, returning
/// the tokens between (and around) each occurrence. The separator itself is
/// not included in any token.
///
/// Returns an empty `Vec` if `sep` is empty or `s` is empty.
/// C: `sdssplitlen` (sds.c:880-925)
///
/// PORT NOTE: The C function returns `NULL` for empty input / empty separator.
/// In Rust we return an empty `Vec`. `sdsfreesplitres` (drop the tokens) maps
/// to `Drop` on the returned `Vec<RedisString>`.
pub fn split_len(s: &[u8], sep: &[u8]) -> Vec<RedisString> {
    if sep.is_empty() || s.is_empty() {
        return Vec::new();
    }
    let mut tokens = Vec::new();
    let mut start = 0usize;
    let mut j = 0usize;
    let len = s.len();
    let seplen = sep.len();

    while j + seplen <= len {
        if s[j..].starts_with(sep) {
            tokens.push(RedisString::from_bytes(&s[start..j]));
            start = j + seplen;
            j = start;
        } else {
            j += 1;
        }
    }
    tokens.push(RedisString::from_bytes(&s[start..]));
    tokens
}

// ──────────────────────────────────────────────────────────────────────────
// Shell-style argument parsing
// ──────────────────────────────────────────────────────────────────────────

/// Splits `line` into shell-style argument tokens.
///
/// Supports:
/// - Double-quoted strings with escape sequences:
///   `\n`, `\r`, `\t`, `\b`, `\a`, `\\`, `\"`, `\xNN`
/// - Single-quoted strings with `\'` escape only
/// - Unquoted tokens (delimited by ASCII whitespace or end of input)
///
/// Returns `None` if `line` contains unbalanced or unterminated quotes.
/// Returns `Some(vec![])` for empty / whitespace-only input.
/// C: `sdssplitargs` (sds.c:1187-1189) → `sdsnsplitargs_internal`
pub fn split_args(line: &[u8]) -> Option<Vec<RedisString>> {
    split_args_internal(line, line.len())
}

/// Like `split_args` but processes only the first `len` bytes of `line`.
/// C: `sdsnsplitargs` (sds.c:1191-1194)
pub fn nsplit_args(line: &[u8], len: usize) -> Option<Vec<RedisString>> {
    split_args_internal(line, len.min(line.len()))
}

/// Internal implementation for both `split_args` and `nsplit_args`.
/// C: `sdsnsplitargs_internal` (sds.c:1150-1185)
fn split_args_internal(line: &[u8], effective_len: usize) -> Option<Vec<RedisString>> {
    let mut result = Vec::new();
    let mut pos = 0usize;
    let input = &line[..effective_len];

    while pos < input.len() && input[pos] != 0 {
        while pos < input.len() && input[pos] != 0 && is_whitespace_byte(input[pos]) {
            pos += 1;
        }
        if pos >= input.len() || input[pos] == 0 {
            break;
        }
        match parse_single_arg(&input[pos..]) {
            None => return None,
            Some((token, consumed)) => {
                result.push(RedisString::from_vec(token));
                pos += consumed;
            }
        }
    }

    Some(result)
}

/// Parses one shell-style argument from the beginning of `input`.
///
/// Returns `Some((bytes, consumed))` on success, where `consumed` is the
/// number of bytes of `input` that were read (including any terminating
/// whitespace character but not a terminating NUL).
/// Returns `None` on unterminated quoted string.
/// C: `sdsparsearg` (sds.c:1044-1126) — two-pass in C (compute length then
/// fill); single-pass in Rust since we push into a Vec directly.
fn parse_single_arg(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut pos = 0usize;
    let mut result = Vec::new();
    let mut in_double_quote = false;
    let mut in_single_quote = false;

    loop {
        if in_double_quote {
            if pos >= input.len() || input[pos] == 0 {
                return None;
            }
            let b = input[pos];
            // \xNN hex escape — needs \, x, and two hex digits.
            if b == b'\\'
                && pos + 3 < input.len()
                && input[pos + 1] == b'x'
                && is_hex_digit(input[pos + 2])
                && is_hex_digit(input[pos + 3])
            {
                let hi = hex_digit_to_int(input[pos + 2]);
                let lo = hex_digit_to_int(input[pos + 3]);
                result.push(hi * 16 + lo);
                pos += 4;
            } else if b == b'\\' && pos + 1 < input.len() {
                let esc = match input[pos + 1] {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'b' => 0x08,
                    b'a' => 0x07,
                    other => other,
                };
                result.push(esc);
                pos += 2;
            } else if b == b'"' {
                in_double_quote = false;
                pos += 1;
            } else {
                result.push(b);
                pos += 1;
            }
        } else if in_single_quote {
            if pos >= input.len() || input[pos] == 0 {
                return None;
            }
            let b = input[pos];
            if b == b'\\' && pos + 1 < input.len() && input[pos + 1] == b'\'' {
                result.push(b'\'');
                pos += 2;
            } else if b == b'\'' {
                in_single_quote = false;
                pos += 1;
            } else {
                result.push(b);
                pos += 1;
            }
        } else {
            if pos >= input.len() {
                break;
            }
            match input[pos] {
                0 => break,
                b' ' | b'\n' | b'\r' | b'\t' => {
                    pos += 1;
                    break;
                }
                b'"' => {
                    in_double_quote = true;
                    pos += 1;
                }
                b'\'' => {
                    in_single_quote = true;
                    pos += 1;
                }
                b => {
                    result.push(b);
                    pos += 1;
                }
            }
        }
    }

    if in_double_quote || in_single_quote {
        return None;
    }

    Some((result, pos))
}

/// Returns `true` if `b` is an ASCII whitespace byte (space, tab, LF, CR).
fn is_whitespace_byte(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

// ──────────────────────────────────────────────────────────────────────────
// Join
// ──────────────────────────────────────────────────────────────────────────

/// Concatenates an array of byte slices, interleaving `sep` between them.
/// C: `sdsjoin` (sds.c:1221-1230)
pub fn join(parts: &[&[u8]], sep: &[u8]) -> RedisString {
    let mut result = RedisString::new();
    for (i, part) in parts.iter().enumerate() {
        result.extend_from_slice(part);
        if i + 1 < parts.len() {
            result.extend_from_slice(sep);
        }
    }
    result
}

/// Concatenates an array of `RedisString`s, interleaving `sep` between them.
/// C: `sdsjoinsds` (sds.c:1233-1242)
pub fn join_sds(parts: &[RedisString], sep: &[u8]) -> RedisString {
    let mut result = RedisString::new();
    for (i, part) in parts.iter().enumerate() {
        result.extend_from_slice(part.as_bytes());
        if i + 1 < parts.len() {
            result.extend_from_slice(sep);
        }
    }
    result
}

// ──────────────────────────────────────────────────────────────────────────
// Template expansion
// ──────────────────────────────────────────────────────────────────────────

/// Expands a template string where `{varname}` placeholders are replaced by
/// values returned by `cb`. A literal `{{` is an escaped single `{`.
///
/// Returns `Err` when:
/// - The template ends inside a `{...}` (premature EOF after `{`)
/// - A `{varname}` has no closing `}`
/// - The callback returns `None` for a variable name
///
/// C: `sdstemplate` (sds.c:1265-1313)
///
/// PORT NOTE: The C API takes `sdstemplate_callback_t cb_func` (a C function
/// pointer) plus a `void *cb_arg`. In Rust, this becomes a closure
/// `F: Fn(&[u8]) -> Option<Vec<u8>>`, which is more ergonomic and
/// avoids the raw-pointer `cb_arg` pattern.
///
/// PORT NOTE: The C `while (*p)` loop stops at the first NUL byte in the
/// template. The Rust version treats NUL as end-of-template for faithful
/// semantic match. Pass a slice without interior NUL bytes for predictable
/// results with non-NUL-terminated templates.
pub fn template<F>(tmpl: &[u8], cb: F) -> Result<RedisString, RedisError>
where
    F: Fn(&[u8]) -> Option<Vec<u8>>,
{
    let mut result = RedisString::new();
    let mut pos = 0usize;

    while pos < tmpl.len() && tmpl[pos] != 0 {
        match tmpl[pos..].iter().position(|&b| b == b'{') {
            None => {
                result.extend_from_slice(&tmpl[pos..]);
                break;
            }
            Some(rel) => {
                let open_pos = pos + rel;
                if open_pos > pos {
                    result.extend_from_slice(&tmpl[pos..open_pos]);
                }
                let after_open = open_pos + 1;
                if after_open >= tmpl.len() {
                    return Err(RedisError::runtime(
                        b"ERR template: premature end of template after '{'",
                    ));
                }
                if tmpl[after_open] == b'{' {
                    result.push(b'{');
                    pos = after_open + 1;
                    continue;
                }
                match tmpl[after_open..].iter().position(|&b| b == b'}') {
                    None => {
                        return Err(RedisError::runtime(
                            b"ERR template: unclosed variable placeholder",
                        ));
                    }
                    Some(rel_close) => {
                        let close_pos = after_open + rel_close;
                        let varname = &tmpl[after_open..close_pos];
                        match cb(varname) {
                            None => {
                                return Err(RedisError::runtime(
                                    b"ERR template: callback returned None for variable",
                                ));
                            }
                            Some(value) => {
                                result.extend_from_slice(&value);
                                pos = close_pos + 1;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(result)
}

// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_long_long_zero() {
        assert_eq!(from_long_long(0).as_bytes(), b"0");
    }

    #[test]
    fn from_long_long_positive() {
        assert_eq!(from_long_long(12345).as_bytes(), b"12345");
    }

    #[test]
    fn from_long_long_negative() {
        assert_eq!(from_long_long(-42).as_bytes(), b"-42");
    }

    #[test]
    fn from_long_long_min() {
        assert_eq!(from_long_long(i64::MIN).as_bytes(), b"-9223372036854775808");
    }

    #[test]
    fn trim_both_ends() {
        let mut s = RedisString::from_bytes(b"  hello  ");
        trim(&mut s, b" ");
        assert_eq!(s.as_bytes(), b"hello");
    }

    #[test]
    fn trim_all_trimmed() {
        let mut s = RedisString::from_bytes(b"   ");
        trim(&mut s, b" ");
        assert!(s.is_empty());
    }

    #[test]
    fn substr_basic() {
        let mut s = RedisString::from_bytes(b"hello");
        substr(&mut s, 1, 3);
        assert_eq!(s.as_bytes(), b"ell");
    }

    #[test]
    fn range_negative_indices() {
        let mut s = RedisString::from_bytes(b"hello");
        range(&mut s, 1, -1);
        assert_eq!(s.as_bytes(), b"ello");
    }

    #[test]
    fn to_lower_ascii() {
        let mut s = RedisString::from_bytes(b"HELLO");
        to_lower(&mut s);
        assert_eq!(s.as_bytes(), b"hello");
    }

    #[test]
    fn split_len_basic() {
        let tokens = split_len(b"foo,bar,baz", b",");
        let strs: Vec<&[u8]> = tokens.iter().map(|s| s.as_bytes()).collect();
        assert_eq!(strs, vec![b"foo", b"bar", b"baz"]);
    }

    #[test]
    fn split_len_empty_sep_returns_empty() {
        assert!(split_len(b"hello", b"").is_empty());
    }

    #[test]
    fn split_args_basic() {
        let args = split_args(b"foo bar baz").unwrap();
        let strs: Vec<&[u8]> = args.iter().map(|s| s.as_bytes()).collect();
        assert_eq!(strs, vec![b"foo", b"bar", b"baz"]);
    }

    #[test]
    fn split_args_quoted() {
        let args = split_args(b"\"hello world\" next").unwrap();
        assert_eq!(args[0].as_bytes(), b"hello world");
        assert_eq!(args[1].as_bytes(), b"next");
    }

    #[test]
    fn split_args_unbalanced_quote_returns_none() {
        assert!(split_args(b"\"unbalanced").is_none());
    }

    #[test]
    fn split_args_hex_escape() {
        let args = split_args(b"\"\\x41\\x42\"").unwrap();
        assert_eq!(args[0].as_bytes(), b"AB");
    }

    #[test]
    fn cat_repr_basic() {
        let mut s = RedisString::new();
        cat_repr(&mut s, b"hello");
        assert_eq!(s.as_bytes(), b"\"hello\"");
    }

    #[test]
    fn cat_repr_newline() {
        let mut s = RedisString::new();
        cat_repr(&mut s, b"a\nb");
        assert_eq!(s.as_bytes(), b"\"a\\nb\"");
    }

    #[test]
    fn template_basic() {
        let result = template(b"Hello {name}!", |var| {
            if var == b"name" {
                Some(b"World".to_vec())
            } else {
                None
            }
        })
        .unwrap();
        assert_eq!(result.as_bytes(), b"Hello World!");
    }

    #[test]
    fn template_escaped_brace() {
        // C: sdstemplate only escapes '{{' → '{'; '}}' is two literal '}' chars.
        // Template "{{literal" → "{literal" (the second '{' triggers the escape,
        // then the rest is copied verbatim).
        let result = template(b"{{literal", |_| None).unwrap();
        assert_eq!(result.as_bytes(), b"{literal");
    }

    #[test]
    fn template_unclosed_returns_err() {
        let r = template(b"Hello {name", |_| None);
        assert!(r.is_err());
    }

    #[test]
    fn join_basic() {
        let parts: Vec<&[u8]> = vec![b"a", b"b", b"c"];
        let result = join(&parts, b",");
        assert_eq!(result.as_bytes(), b"a,b,c");
    }

    #[test]
    fn map_chars_basic() {
        let mut s = RedisString::from_bytes(b"hello");
        map_chars(&mut s, b"ho", b"01");
        assert_eq!(s.as_bytes(), b"0ell1");
    }

    #[test]
    fn grow_zero_extends() {
        let mut s = RedisString::from_bytes(b"ab");
        grow_zero(&mut s, 5);
        assert_eq!(s.as_bytes(), b"ab\x00\x00\x00");
    }

    #[test]
    fn grow_zero_no_shrink() {
        let mut s = RedisString::from_bytes(b"hello");
        grow_zero(&mut s, 3);
        assert_eq!(s.as_bytes(), b"hello");
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/sds.c (1314 lines, 54 functions)
//                  src/sds.h (300 lines, inline functions + type definitions)
//   target_crate:  redis-types
//   confidence:    medium
//   todos:         4
//   port_notes:    6
//   unsafe_blocks: 0
//   notes:         SDS header-type machinery (sdshdr5..64, sdsHdrSize, sdswrite,
//                  sdsReqType, sdsIncrLen, sdsAllocPtr, sdssetlen, etc.) is NOT
//                  ported — Vec<u8> provides equivalent guarantees transparently.
//                  All semantic operations (trim, range, substr, split, join,
//                  template, repr, mapchars, splitargs) are faithfully translated.
//                  sdscatfmt (C varargs) is left as TODO(port) for Phase B.
//                  make_room_for / shrink_to_fit are stubs pending
//                  RedisString::reserve() / shrink_to_fit() architect decision.
// ──────────────────────────────────────────────────────────────────────────
