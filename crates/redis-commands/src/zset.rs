//! Sorted-set (`zset`) command implementations.
//!
//! Covers the byte-exact wire surface of ZADD, ZSCORE, ZMSCORE, ZCARD,
//! ZINCRBY, ZRANGE, ZRANGEBYSCORE, ZREVRANGE, ZREVRANGEBYSCORE, ZRANK,
//! ZREVRANK, ZREM, ZCOUNT, ZPOPMIN, ZPOPMAX, ZREMRANGEBYRANK, and
//! ZREMRANGEBYSCORE for Round 5.
//!
//! C source: `reference/valkey/src/t_zset.c`.
//!
//! # Storage shape
//!
//! Round 5 uses the pragmatic `ObjectKind::ZSet(ZSetEncoding::Inline(_))`
//! encoding from `redis-core::object` — an `InlineZSet` whose dual
//! `HashMap` + `BTreeSet` mirror the dict + zskiplist pair in real
//! Redis. Phase 4 swaps this for the real `redis_ds::ZSet` once that
//! crate ships the listpack / skiplist primitives.
//!
//! # Architect items
//!
//! TODO(architect): swap the `Inline` encoding for real `ListPack` /
//! `SkipList` types from `redis-ds` once Phase 4 makes them usable.
//!
//! TODO(architect): score formatter parity — Rust's default
//! `f64::to_string` differs from C's `humanfriendly_number_to_string`
//! (`%.17g` + trailing-zero trim) on some edge cases. The smoke corpus
//! sticks to scores whose representation matches under both formatters.
//!
//! TODO(architect): ZRANGEBYLEX / ZREVRANGEBYLEX / ZLEXCOUNT /
//! ZREMRANGEBYLEX / ZRANGESTORE / ZUNIONSTORE / ZINTERSTORE /
//! ZDIFFSTORE / ZUNION / ZINTER / ZDIFF / ZINTERCARD / ZRANDMEMBER /
//! ZMPOP / ZSCAN / BZPOPMIN / BZPOPMAX land in follow-on rounds.

use redis_core::command_context::CommandContext;
use redis_core::object::{InlineZSet, RedisObject};
use redis_types::{RedisError, RedisResult, RedisString};

/// Parse a score expressed in Redis's float syntax.
///
/// Accepts ASCII decimal, scientific notation, and `+inf` / `-inf` /
/// `inf` (case-insensitive). Rejects NaN, whitespace, empty strings,
/// and any trailing garbage with the canonical Redis error reply.
fn parse_score(bytes: &[u8]) -> Result<f64, RedisError> {
    if bytes.is_empty() {
        return Err(RedisError::not_float());
    }
    let s = core::str::from_utf8(bytes).map_err(|_| RedisError::not_float())?;
    if s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace) {
        return Err(RedisError::not_float());
    }
    let lower = s.to_ascii_lowercase();
    if lower == "inf" || lower == "+inf" || lower == "infinity" || lower == "+infinity" {
        return Ok(f64::INFINITY);
    }
    if lower == "-inf" || lower == "-infinity" {
        return Ok(f64::NEG_INFINITY);
    }
    if lower == "nan" || lower == "+nan" || lower == "-nan" {
        return Err(RedisError::not_float());
    }
    let v: f64 = s.parse().map_err(|_| RedisError::not_float())?;
    if v.is_nan() {
        return Err(RedisError::not_float());
    }
    Ok(v)
}

/// Parse a strict base-10 `i64` matching Redis's accept rules.
fn parse_strict_i64(bytes: &[u8]) -> Result<i64, RedisError> {
    if bytes.is_empty() {
        return Err(RedisError::not_integer());
    }
    let s = core::str::from_utf8(bytes).map_err(|_| RedisError::not_integer())?;
    if s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace) {
        return Err(RedisError::not_integer());
    }
    s.parse::<i64>().map_err(|_| RedisError::not_integer())
}

/// Parse one side of a score range, handling the exclusive `(` prefix.
///
/// Returns `(score, exclusive)`.
fn parse_score_range(bytes: &[u8]) -> Result<(f64, bool), RedisError> {
    let (excl, rest) = match bytes.first() {
        Some(b'(') => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    let score = parse_score(rest).map_err(|_| {
        RedisError::runtime(b"ERR min or max is not a float")
    })?;
    Ok((score, excl))
}

/// Format a score for bulk-string replies.
///
/// Uses Rust's default `f64::to_string` plus an explicit `inf` / `-inf`
/// short-form to match Redis's `humanfriendly_number_to_string` output.
///
/// TODO(architect): full `%.17g` parity once a dedicated formatter
/// helper is wired in.
fn format_score(score: f64) -> Vec<u8> {
    if score.is_infinite() {
        if score > 0.0 {
            b"inf".to_vec()
        } else {
            b"-inf".to_vec()
        }
    } else if score == 0.0 {
        b"0".to_vec()
    } else if score == score.trunc() && score.abs() < 1e17 {
        format!("{}", score as i64).into_bytes()
    } else {
        format!("{}", score).into_bytes()
    }
}

/// Borrow the inner `InlineZSet` of a zset-encoded `RedisObject`,
/// raising `WRONGTYPE` if `obj` is any other kind.
fn as_zset_ref(
    obj: Option<&RedisObject>,
) -> Result<Option<&InlineZSet>, RedisError> {
    match obj {
        None => Ok(None),
        Some(o) => o.zset().map(Some).ok_or_else(RedisError::wrong_type),
    }
}

/// Mutable counterpart of `as_zset_ref`.
fn as_zset_mut(
    obj: Option<&mut RedisObject>,
) -> Result<Option<&mut InlineZSet>, RedisError> {
    match obj {
        None => Ok(None),
        Some(o) => {
            if o.is_zset() {
                Ok(o.zset_mut())
            } else {
                Err(RedisError::wrong_type())
            }
        }
    }
}

/// Resolve `start`/`stop` for inclusive range queries.
///
/// Mirrors `zslGetRangeInLen` — clamps negatives to zero, clamps
/// `stop >= len` to `len-1`, and returns `None` when the range is
/// empty after clamping.
fn clamp_rank_range(start: i64, stop: i64, len: i64) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let s = if start < 0 { (len + start).max(0) } else { start };
    let e = if stop < 0 { len + stop } else { stop };
    if s >= len || e < s {
        return None;
    }
    let e = e.min(len - 1);
    Some((s as usize, e as usize))
}

/// Delete the key when its zset has become empty.
fn delete_if_empty(ctx: &mut CommandContext, key: &RedisString) {
    let empty = matches!(
        ctx.db().lookup_key_read(key),
        Some(o) if o.zset().map(|z| z.is_empty()).unwrap_or(false)
    );
    if empty {
        ctx.db_mut().sync_delete(key);
    }
}

/// ZADD key [NX|XX] [GT|LT] [CH] [INCR] score member [score member ...]
///
/// Adds one or more `(score, member)` pairs to the sorted set at `key`,
/// creating the key when absent. Without `CH` the reply is the number
/// of *new* members; with `CH` it counts newly-added plus updated
/// scores. The `INCR` flag toggles single-pair increment semantics.
pub fn zadd_command(ctx: &mut CommandContext) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 4 {
        return Err(RedisError::wrong_number_of_args(b"zadd"));
    }

    let key = ctx.arg_owned(1usize)?;
    let mut nx = false;
    let mut xx = false;
    let mut gt = false;
    let mut lt = false;
    let mut ch = false;
    let mut incr = false;

    let mut idx = 2usize;
    while idx < argc {
        let opt = ctx.arg(idx)?;
        let bytes = opt.as_bytes();
        if bytes.eq_ignore_ascii_case(b"NX") {
            nx = true;
        } else if bytes.eq_ignore_ascii_case(b"XX") {
            xx = true;
        } else if bytes.eq_ignore_ascii_case(b"GT") {
            gt = true;
        } else if bytes.eq_ignore_ascii_case(b"LT") {
            lt = true;
        } else if bytes.eq_ignore_ascii_case(b"CH") {
            ch = true;
        } else if bytes.eq_ignore_ascii_case(b"INCR") {
            incr = true;
        } else {
            break;
        }
        idx += 1;
    }

    if nx && xx {
        return Err(RedisError::runtime(
            b"ERR XX and NX options at the same time are not compatible",
        ));
    }
    if (gt || lt) && nx {
        return Err(RedisError::runtime(
            b"ERR GT, LT, and/or NX options at the same time are not compatible",
        ));
    }
    if gt && lt {
        return Err(RedisError::runtime(
            b"ERR GT, LT, and/or NX options at the same time are not compatible",
        ));
    }

    let remaining = argc - idx;
    if remaining == 0 || remaining % 2 != 0 {
        return Err(RedisError::syntax(b"syntax error"));
    }
    if incr && remaining != 2 {
        return Err(RedisError::runtime(
            b"ERR INCR option supports a single increment-element pair",
        ));
    }

    let mut pairs: Vec<(f64, RedisString)> = Vec::with_capacity(remaining / 2);
    let mut j = idx;
    while j < argc {
        let score = parse_score(ctx.arg(j)?.as_bytes())?;
        let member = ctx.arg_owned(j + 1)?;
        pairs.push((score, member));
        j += 2;
    }

    if let Some(existing) = ctx.db().lookup_key_read(&key) {
        if !existing.is_zset() {
            return Err(RedisError::wrong_type());
        }
    }

    if ctx.db().lookup_key_read(&key).is_none() {
        if xx {
            if incr {
                return ctx.reply_null_bulk();
            }
            return ctx.reply_integer(0);
        }
        let obj = RedisObject::new_zset();
        ctx.db_mut().set_key(key.clone(), obj, 0);
    }

    let mut added: i64 = 0;
    let mut changed: i64 = 0;
    let mut incr_reply: Option<Option<f64>> = None;

    let zset = ctx
        .db_mut()
        .lookup_key_write(&key)
        .and_then(|o| o.zset_mut())
        .expect("zset created or pre-validated above");

    for (score, member) in pairs {
        let prev = zset.score(&member);
        match prev {
            None => {
                if xx {
                    if incr {
                        incr_reply = Some(None);
                    }
                    continue;
                }
                let final_score = score;
                zset.upsert(member, final_score);
                added += 1;
                changed += 1;
                if incr {
                    incr_reply = Some(Some(final_score));
                }
            }
            Some(prev_score) => {
                if nx {
                    if incr {
                        incr_reply = Some(None);
                    }
                    continue;
                }
                let candidate = if incr { prev_score + score } else { score };
                if candidate.is_nan() {
                    return Err(RedisError::runtime(
                        b"ERR resulting score is not a number (NaN)",
                    ));
                }
                if gt && !(candidate > prev_score) {
                    if incr {
                        incr_reply = Some(None);
                    }
                    continue;
                }
                if lt && !(candidate < prev_score) {
                    if incr {
                        incr_reply = Some(None);
                    }
                    continue;
                }
                if candidate.to_bits() != prev_score.to_bits() {
                    zset.upsert(member, candidate);
                    changed += 1;
                }
                if incr {
                    incr_reply = Some(Some(candidate));
                }
            }
        }
    }

    delete_if_empty(ctx, &key);

    if incr {
        match incr_reply {
            Some(Some(score)) => ctx.reply_bulk(&format_score(score)),
            _ => ctx.reply_null_bulk(),
        }
    } else if ch {
        ctx.reply_integer(changed)
    } else {
        ctx.reply_integer(added)
    }
}

/// ZSCORE key member
pub fn zscore_command(ctx: &mut CommandContext) -> RedisResult<()> {
    if ctx.arg_count() != 3 {
        return Err(RedisError::wrong_number_of_args(b"zscore"));
    }
    let key = ctx.arg_owned(1usize)?;
    let member = ctx.arg_owned(2usize)?;
    let score = match as_zset_ref(ctx.db().lookup_key_read(&key))? {
        None => None,
        Some(z) => z.score(&member),
    };
    match score {
        Some(s) => ctx.reply_bulk(&format_score(s)),
        None => ctx.reply_null_bulk(),
    }
}

/// ZMSCORE key member [member ...]
pub fn zmscore_command(ctx: &mut CommandContext) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 3 {
        return Err(RedisError::wrong_number_of_args(b"zmscore"));
    }
    let key = ctx.arg_owned(1usize)?;
    let mut members: Vec<RedisString> = Vec::with_capacity(argc - 2);
    for j in 2..argc {
        members.push(ctx.arg_owned(j)?);
    }
    let scores: Vec<Option<f64>> = match as_zset_ref(ctx.db().lookup_key_read(&key))? {
        None => members.iter().map(|_| None).collect(),
        Some(z) => members.iter().map(|m| z.score(m)).collect(),
    };
    ctx.reply_array_header(scores.len())?;
    for s in scores {
        match s {
            Some(v) => ctx.reply_bulk(&format_score(v))?,
            None => ctx.reply_null_bulk()?,
        }
    }
    Ok(())
}

/// ZCARD key
pub fn zcard_command(ctx: &mut CommandContext) -> RedisResult<()> {
    if ctx.arg_count() != 2 {
        return Err(RedisError::wrong_number_of_args(b"zcard"));
    }
    let key = ctx.arg_owned(1usize)?;
    let len = match as_zset_ref(ctx.db().lookup_key_read(&key))? {
        None => 0,
        Some(z) => z.len() as i64,
    };
    ctx.reply_integer(len)
}

/// ZINCRBY key delta member
pub fn zincrby_command(ctx: &mut CommandContext) -> RedisResult<()> {
    if ctx.arg_count() != 4 {
        return Err(RedisError::wrong_number_of_args(b"zincrby"));
    }
    let key = ctx.arg_owned(1usize)?;
    let delta = parse_score(ctx.arg(2)?.as_bytes())?;
    let member = ctx.arg_owned(3usize)?;

    if let Some(existing) = ctx.db().lookup_key_read(&key) {
        if !existing.is_zset() {
            return Err(RedisError::wrong_type());
        }
    }
    if ctx.db().lookup_key_read(&key).is_none() {
        ctx.db_mut().set_key(key.clone(), RedisObject::new_zset(), 0);
    }
    let zset = ctx
        .db_mut()
        .lookup_key_write(&key)
        .and_then(|o| o.zset_mut())
        .expect("zset created above");
    let new_score = match zset.score(&member) {
        Some(prev) => prev + delta,
        None => delta,
    };
    if new_score.is_nan() {
        return Err(RedisError::runtime(
            b"ERR resulting score is not a number (NaN)",
        ));
    }
    zset.upsert(member, new_score);
    ctx.reply_bulk(&format_score(new_score))
}

/// ZREM key member [member ...]
pub fn zrem_command(ctx: &mut CommandContext) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 3 {
        return Err(RedisError::wrong_number_of_args(b"zrem"));
    }
    let key = ctx.arg_owned(1usize)?;
    let mut members: Vec<RedisString> = Vec::with_capacity(argc - 2);
    for j in 2..argc {
        members.push(ctx.arg_owned(j)?);
    }
    let removed = {
        let zset = match as_zset_mut(ctx.db_mut().lookup_key_write(&key))? {
            None => return ctx.reply_integer(0),
            Some(z) => z,
        };
        let mut count: i64 = 0;
        for m in members {
            if zset.remove(&m).is_some() {
                count += 1;
            }
        }
        count
    };
    delete_if_empty(ctx, &key);
    ctx.reply_integer(removed)
}

/// ZRANK / ZREVRANK shared body.
fn rank_inner(ctx: &mut CommandContext, reverse: bool, cmd: &[u8]) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc != 3 && argc != 4 {
        return Err(RedisError::wrong_number_of_args(cmd));
    }
    let withscore = if argc == 4 {
        let opt = ctx.arg(3)?;
        if !opt.as_bytes().eq_ignore_ascii_case(b"WITHSCORE") {
            return Err(RedisError::syntax(b"syntax error"));
        }
        true
    } else {
        false
    };
    let key = ctx.arg_owned(1usize)?;
    let member = ctx.arg_owned(2usize)?;

    let result: Option<(i64, f64)> = match as_zset_ref(ctx.db().lookup_key_read(&key))? {
        None => None,
        Some(z) => {
            let n = z.len() as i64;
            let mut found: Option<(i64, f64)> = None;
            for (i, (score, m)) in z.iter_ascending().enumerate() {
                if m == &member {
                    let rank = if reverse {
                        n - 1 - (i as i64)
                    } else {
                        i as i64
                    };
                    found = Some((rank, score));
                    break;
                }
            }
            found
        }
    };

    match (result, withscore) {
        (None, false) => ctx.reply_null_bulk(),
        (None, true) => ctx.reply_null_array(),
        (Some((rank, _)), false) => ctx.reply_integer(rank),
        (Some((rank, score)), true) => {
            ctx.reply_array_header(2usize)?;
            ctx.reply_integer(rank)?;
            ctx.reply_bulk(&format_score(score))
        }
    }
}

/// ZRANK key member [WITHSCORE]
pub fn zrank_command(ctx: &mut CommandContext) -> RedisResult<()> {
    rank_inner(ctx, false, b"zrank")
}

/// ZREVRANK key member [WITHSCORE]
pub fn zrevrank_command(ctx: &mut CommandContext) -> RedisResult<()> {
    rank_inner(ctx, true, b"zrevrank")
}

/// ZCOUNT key min max
pub fn zcount_command(ctx: &mut CommandContext) -> RedisResult<()> {
    if ctx.arg_count() != 4 {
        return Err(RedisError::wrong_number_of_args(b"zcount"));
    }
    let key = ctx.arg_owned(1usize)?;
    let (min, min_excl) = parse_score_range(ctx.arg(2)?.as_bytes())?;
    let (max, max_excl) = parse_score_range(ctx.arg(3)?.as_bytes())?;
    let count: i64 = match as_zset_ref(ctx.db().lookup_key_read(&key))? {
        None => 0,
        Some(z) => z
            .iter_ascending()
            .filter(|(s, _)| score_in_range(*s, min, min_excl, max, max_excl))
            .count() as i64,
    };
    ctx.reply_integer(count)
}

/// Inclusive/exclusive score-range membership test.
fn score_in_range(s: f64, min: f64, min_excl: bool, max: f64, max_excl: bool) -> bool {
    let lower_ok = if min_excl { s > min } else { s >= min };
    let upper_ok = if max_excl { s < max } else { s <= max };
    lower_ok && upper_ok
}

/// Common rank-based range body shared by ZRANGE (default) and ZREVRANGE.
fn range_by_rank(
    ctx: &mut CommandContext,
    key: &RedisString,
    start: i64,
    stop: i64,
    reverse: bool,
    withscores: bool,
) -> RedisResult<()> {
    let entries: Vec<(f64, RedisString)> = match as_zset_ref(ctx.db().lookup_key_read(key))? {
        None => Vec::new(),
        Some(z) => {
            let len = z.len() as i64;
            match clamp_rank_range(start, stop, len) {
                None => Vec::new(),
                Some((lo, hi)) => {
                    if reverse {
                        z.iter_ascending()
                            .rev()
                            .skip(lo)
                            .take(hi - lo + 1)
                            .map(|(s, m)| (s, m.clone()))
                            .collect()
                    } else {
                        z.iter_ascending()
                            .skip(lo)
                            .take(hi - lo + 1)
                            .map(|(s, m)| (s, m.clone()))
                            .collect()
                    }
                }
            }
        }
    };
    emit_range_reply(ctx, entries, withscores)
}

/// Reply with `entries` as either a flat member array or interleaved
/// member/score array, depending on `withscores`.
fn emit_range_reply(
    ctx: &mut CommandContext,
    entries: Vec<(f64, RedisString)>,
    withscores: bool,
) -> RedisResult<()> {
    let len = if withscores {
        entries.len() * 2
    } else {
        entries.len()
    };
    ctx.reply_array_header(len)?;
    for (score, member) in entries {
        ctx.reply_bulk_string(member)?;
        if withscores {
            ctx.reply_bulk(&format_score(score))?;
        }
    }
    Ok(())
}

/// Common score-range body shared by ZRANGEBYSCORE and ZREVRANGEBYSCORE.
fn range_by_score(
    ctx: &mut CommandContext,
    key: &RedisString,
    min: f64,
    min_excl: bool,
    max: f64,
    max_excl: bool,
    reverse: bool,
    offset: i64,
    count: i64,
    withscores: bool,
) -> RedisResult<()> {
    let entries: Vec<(f64, RedisString)> = match as_zset_ref(ctx.db().lookup_key_read(key))? {
        None => Vec::new(),
        Some(z) => {
            let all: Vec<(f64, RedisString)> = z
                .iter_ascending()
                .filter(|(s, _)| score_in_range(*s, min, min_excl, max, max_excl))
                .map(|(s, m)| (s, m.clone()))
                .collect();
            let iter: Box<dyn Iterator<Item = (f64, RedisString)>> = if reverse {
                Box::new(all.into_iter().rev())
            } else {
                Box::new(all.into_iter())
            };
            let skipped: Box<dyn Iterator<Item = (f64, RedisString)>> = if offset > 0 {
                Box::new(iter.skip(offset as usize))
            } else {
                iter
            };
            if count < 0 {
                skipped.collect()
            } else {
                skipped.take(count as usize).collect()
            }
        }
    };
    emit_range_reply(ctx, entries, withscores)
}

/// ZRANGE key start stop [BYSCORE|BYLEX] [REV] [LIMIT offset count] [WITHSCORES]
pub fn zrange_command(ctx: &mut CommandContext) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 4 {
        return Err(RedisError::wrong_number_of_args(b"zrange"));
    }
    let key = ctx.arg_owned(1usize)?;
    let start_bytes = ctx.arg_owned(2usize)?;
    let stop_bytes = ctx.arg_owned(3usize)?;

    let mut by_score = false;
    let mut by_lex = false;
    let mut reverse = false;
    let mut withscores = false;
    let mut offset: i64 = 0;
    let mut count: i64 = -1;
    let mut have_limit = false;

    let mut idx = 4usize;
    while idx < argc {
        let opt = ctx.arg(idx)?;
        let bytes = opt.as_bytes();
        if bytes.eq_ignore_ascii_case(b"BYSCORE") {
            by_score = true;
            idx += 1;
        } else if bytes.eq_ignore_ascii_case(b"BYLEX") {
            by_lex = true;
            idx += 1;
        } else if bytes.eq_ignore_ascii_case(b"REV") {
            reverse = true;
            idx += 1;
        } else if bytes.eq_ignore_ascii_case(b"WITHSCORES") {
            withscores = true;
            idx += 1;
        } else if bytes.eq_ignore_ascii_case(b"LIMIT") {
            if idx + 2 >= argc {
                return Err(RedisError::syntax(b"syntax error"));
            }
            offset = parse_strict_i64(ctx.arg(idx + 1)?.as_bytes())?;
            count = parse_strict_i64(ctx.arg(idx + 2)?.as_bytes())?;
            have_limit = true;
            idx += 3;
        } else {
            return Err(RedisError::syntax(b"syntax error"));
        }
    }

    if by_lex {
        return Err(RedisError::syntax(
            b"syntax error, BYLEX not implemented yet in this port",
        ));
    }
    if have_limit && !by_score && !by_lex {
        return Err(RedisError::runtime(
            b"ERR syntax error, LIMIT is only supported in combination with either BYSCORE or BYLEX",
        ));
    }

    if by_score {
        let (a_score, a_excl) = parse_score_range(start_bytes.as_bytes())?;
        let (b_score, b_excl) = parse_score_range(stop_bytes.as_bytes())?;
        let (min, min_excl, max, max_excl) = if reverse {
            (b_score, b_excl, a_score, a_excl)
        } else {
            (a_score, a_excl, b_score, b_excl)
        };
        return range_by_score(
            ctx, &key, min, min_excl, max, max_excl, reverse, offset, count, withscores,
        );
    }

    let start = parse_strict_i64(start_bytes.as_bytes())?;
    let stop = parse_strict_i64(stop_bytes.as_bytes())?;
    range_by_rank(ctx, &key, start, stop, reverse, withscores)
}

/// ZREVRANGE key start stop [WITHSCORES]
pub fn zrevrange_command(ctx: &mut CommandContext) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc != 4 && argc != 5 {
        return Err(RedisError::wrong_number_of_args(b"zrevrange"));
    }
    let key = ctx.arg_owned(1usize)?;
    let start = parse_strict_i64(ctx.arg(2)?.as_bytes())?;
    let stop = parse_strict_i64(ctx.arg(3)?.as_bytes())?;
    let withscores = if argc == 5 {
        let opt = ctx.arg(4)?;
        if !opt.as_bytes().eq_ignore_ascii_case(b"WITHSCORES") {
            return Err(RedisError::syntax(b"syntax error"));
        }
        true
    } else {
        false
    };
    range_by_rank(ctx, &key, start, stop, true, withscores)
}

/// Shared body for ZRANGEBYSCORE and ZREVRANGEBYSCORE.
fn rangebyscore_inner(ctx: &mut CommandContext, reverse: bool, cmd: &[u8]) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 4 {
        return Err(RedisError::wrong_number_of_args(cmd));
    }
    let key = ctx.arg_owned(1usize)?;
    let arg_a = ctx.arg_owned(2usize)?;
    let arg_b = ctx.arg_owned(3usize)?;

    let mut withscores = false;
    let mut offset: i64 = 0;
    let mut count: i64 = -1;
    let mut idx = 4usize;
    while idx < argc {
        let opt = ctx.arg(idx)?;
        let bytes = opt.as_bytes();
        if bytes.eq_ignore_ascii_case(b"WITHSCORES") {
            withscores = true;
            idx += 1;
        } else if bytes.eq_ignore_ascii_case(b"LIMIT") {
            if idx + 2 >= argc {
                return Err(RedisError::syntax(b"syntax error"));
            }
            offset = parse_strict_i64(ctx.arg(idx + 1)?.as_bytes())?;
            count = parse_strict_i64(ctx.arg(idx + 2)?.as_bytes())?;
            idx += 3;
        } else {
            return Err(RedisError::syntax(b"syntax error"));
        }
    }

    let (a_score, a_excl) = parse_score_range(arg_a.as_bytes())?;
    let (b_score, b_excl) = parse_score_range(arg_b.as_bytes())?;
    let (min, min_excl, max, max_excl) = if reverse {
        (b_score, b_excl, a_score, a_excl)
    } else {
        (a_score, a_excl, b_score, b_excl)
    };
    range_by_score(
        ctx, &key, min, min_excl, max, max_excl, reverse, offset, count, withscores,
    )
}

/// ZRANGEBYSCORE key min max [WITHSCORES] [LIMIT offset count]
pub fn zrangebyscore_command(ctx: &mut CommandContext) -> RedisResult<()> {
    rangebyscore_inner(ctx, false, b"zrangebyscore")
}

/// ZREVRANGEBYSCORE key max min [WITHSCORES] [LIMIT offset count]
pub fn zrevrangebyscore_command(ctx: &mut CommandContext) -> RedisResult<()> {
    rangebyscore_inner(ctx, true, b"zrevrangebyscore")
}

/// Shared body for ZPOPMIN and ZPOPMAX.
fn popminmax_inner(ctx: &mut CommandContext, reverse: bool, cmd: &[u8]) -> RedisResult<()> {
    let argc = ctx.arg_count();
    if argc < 2 || argc > 3 {
        return Err(RedisError::wrong_number_of_args(cmd));
    }
    let key = ctx.arg_owned(1usize)?;
    let count_arg: Option<i64> = if argc == 3 {
        Some(parse_strict_i64(ctx.arg(2)?.as_bytes())?)
    } else {
        None
    };
    if let Some(n) = count_arg {
        if n < 0 {
            return Err(RedisError::runtime(
                b"ERR value is out of range, must be positive",
            ));
        }
    }

    let popped: Vec<(f64, RedisString)> = {
        let zset = match as_zset_mut(ctx.db_mut().lookup_key_write(&key))? {
            None => Vec::new(),
            Some(z) => {
                let count = count_arg.unwrap_or(1) as usize;
                let take = count.min(z.len());
                let mut targets: Vec<(f64, RedisString)> = Vec::with_capacity(take);
                if reverse {
                    for (score, member) in z.iter_ascending().rev().take(take) {
                        targets.push((score, member.clone()));
                    }
                } else {
                    for (score, member) in z.iter_ascending().take(take) {
                        targets.push((score, member.clone()));
                    }
                }
                for (_, m) in &targets {
                    z.remove(m);
                }
                targets
            }
        };
        zset
    };
    delete_if_empty(ctx, &key);

    match count_arg {
        None => {
            if popped.is_empty() {
                ctx.reply_array_header(0usize)?;
                return Ok(());
            }
            ctx.reply_array_header(2usize)?;
            let (score, member) = popped
                .into_iter()
                .next()
                .expect("popped non-empty");
            ctx.reply_bulk_string(member)?;
            ctx.reply_bulk(&format_score(score))
        }
        Some(_) => {
            ctx.reply_array_header(popped.len() * 2)?;
            for (score, member) in popped {
                ctx.reply_bulk_string(member)?;
                ctx.reply_bulk(&format_score(score))?;
            }
            Ok(())
        }
    }
}

/// ZPOPMIN key [count]
pub fn zpopmin_command(ctx: &mut CommandContext) -> RedisResult<()> {
    popminmax_inner(ctx, false, b"zpopmin")
}

/// ZPOPMAX key [count]
pub fn zpopmax_command(ctx: &mut CommandContext) -> RedisResult<()> {
    popminmax_inner(ctx, true, b"zpopmax")
}

/// ZREMRANGEBYRANK key start stop
pub fn zremrangebyrank_command(ctx: &mut CommandContext) -> RedisResult<()> {
    if ctx.arg_count() != 4 {
        return Err(RedisError::wrong_number_of_args(b"zremrangebyrank"));
    }
    let key = ctx.arg_owned(1usize)?;
    let start = parse_strict_i64(ctx.arg(2)?.as_bytes())?;
    let stop = parse_strict_i64(ctx.arg(3)?.as_bytes())?;
    let removed = {
        let zset = match as_zset_mut(ctx.db_mut().lookup_key_write(&key))? {
            None => return ctx.reply_integer(0),
            Some(z) => z,
        };
        let len = zset.len() as i64;
        let (lo, hi) = match clamp_rank_range(start, stop, len) {
            None => return ctx.reply_integer(0),
            Some(r) => r,
        };
        let to_remove: Vec<RedisString> = zset
            .iter_ascending()
            .skip(lo)
            .take(hi - lo + 1)
            .map(|(_, m)| m.clone())
            .collect();
        let count = to_remove.len() as i64;
        for m in to_remove {
            zset.remove(&m);
        }
        count
    };
    delete_if_empty(ctx, &key);
    ctx.reply_integer(removed)
}

/// ZREMRANGEBYSCORE key min max
pub fn zremrangebyscore_command(ctx: &mut CommandContext) -> RedisResult<()> {
    if ctx.arg_count() != 4 {
        return Err(RedisError::wrong_number_of_args(b"zremrangebyscore"));
    }
    let key = ctx.arg_owned(1usize)?;
    let (min, min_excl) = parse_score_range(ctx.arg(2)?.as_bytes())?;
    let (max, max_excl) = parse_score_range(ctx.arg(3)?.as_bytes())?;
    let removed = {
        let zset = match as_zset_mut(ctx.db_mut().lookup_key_write(&key))? {
            None => return ctx.reply_integer(0),
            Some(z) => z,
        };
        let to_remove: Vec<RedisString> = zset
            .iter_ascending()
            .filter(|(s, _)| score_in_range(*s, min, min_excl, max, max_excl))
            .map(|(_, m)| m.clone())
            .collect();
        let count = to_remove.len() as i64;
        for m in to_remove {
            zset.remove(&m);
        }
        count
    };
    delete_if_empty(ctx, &key);
    ctx.reply_integer(removed)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/t_zset.c
//   target_crate:  redis-commands
//   confidence:    medium
//   todos:         3
//   port_notes:    0
//   unsafe_blocks: 0
//   notes:         Round 5 byte-exact implementations for ZADD, ZSCORE,
//                  ZMSCORE, ZCARD, ZINCRBY, ZRANGE, ZRANGEBYSCORE,
//                  ZREVRANGE, ZREVRANGEBYSCORE, ZRANK, ZREVRANK, ZREM,
//                  ZCOUNT, ZPOPMIN, ZPOPMAX, ZREMRANGEBYRANK, and
//                  ZREMRANGEBYSCORE backed by the pragmatic
//                  ZSetEncoding::Inline encoding from redis-core::object.
//                  Score formatting uses Rust's f64 Display plus integer
//                  shortcut; Phase 4 will install a %.17g-faithful
//                  helper. ZRANGEBYLEX / ZREMRANGEBYLEX / ZLEXCOUNT and
//                  ZUNIONSTORE / ZINTERSTORE / ZDIFFSTORE / ZUNION /
//                  ZINTER / ZDIFF / ZINTERCARD / ZRANDMEMBER / ZMPOP /
//                  ZSCAN / BZPOPMIN / BZPOPMAX land in follow-on rounds.
// ──────────────────────────────────────────────────────────────────────────
