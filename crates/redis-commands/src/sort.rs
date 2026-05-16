//! SORT / SORT_RO command implementations.
//!
//! C source: `reference/valkey/src/sort.c` (622 lines, 5 functions)
//! Crate: `redis-commands` (later phase)
//!
//! Implements the SORT and SORT_RO commands, which are the most complex
//! commands in Valkey.  Supports BY-pattern weighted sorts, GET-pattern
//! value retrieval, LIMIT offset/count, ASC/DESC, ALPHA, and STORE.
//!
//! Key structural changes from C:
//! - The C `sortCompare` comparator reads sort parameters from the global
//!   `server` struct (sort_alpha, sort_desc, sort_bypattern, sort_store).
//!   In Rust we pass a `SortParams` value explicitly so there is no global
//!   state.
//! - `serverSortObject.u` is a C union (`score: f64` | `cmpobj: *robj`).
//!   In Rust this is `SortScore`, a plain enum.
//! - `pqsort` (partial quicksort for LIMIT) is replaced by a full sort
//!   with a PERF note; a proper partial-sort optimisation can be added in
//!   Phase B.
//! - Ref-count management (`incrRefCount`/`decrRefCount`) is eliminated;
//!   Rust ownership handles it.
//!
//! TODO(architect): `CommandContext::db()` and `db_mut()` — need
//! `&mut RedisServer` plumbed through `CommandContext` (Phase 3).
//!
//! TODO(architect): `CommandContext::server()` / `server_mut()` — access
//! to `RedisServer` for `dirty`, `list_max_listpack_size`,
//! `list_compress_depth`, and cluster-mode flags.
//!
//! TODO(architect): `CommandContext::notify_keyspace_event(flags, event, key)`
//! — keyspace notification dispatch blocked on Phase 3.
//!
//! TODO(architect): `CommandContext::signal_modified_key(key)` — WATCH /
//! client-tracking invalidation blocked on Phase 3.
//!
//! TODO(architect): ACL check helper
//! `acl_user_check_cmd_with_unrestricted_key_access(...)` — blocked on ACL
//! layer (later phase).
//!
//! TODO(architect): `RedisDb::lookup_key_read`, `RedisDb::set_key`,
//! `RedisDb::delete` — canonical db-access methods (Phase 3).

use redis_core::command_context::CommandContext;
use redis_core::object::RedisObject;
use redis_types::{RedisError, RedisString};

// ─────────────────────────────────────────────────────────────────────────────
// Sort operation types
// ─────────────────────────────────────────────────────────────────────────────

/// Corresponds to `SORT_OP_GET` in C (`server.h`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SortOpType {
    Get,
}

/// One GET (or future DEL/INCR/DECR) operation to apply to each sorted
/// element.
///
/// C: `serverSortOperation` in `server.h`.
pub(crate) struct SortOperation {
    pub op_type: SortOpType,
    /// The pattern string, e.g. `weight_*` or `obj_*->field`.
    pub pattern: RedisObject,
}

// ─────────────────────────────────────────────────────────────────────────────
// Per-element sort vector entry
// ─────────────────────────────────────────────────────────────────────────────

/// The sort score associated with one element.
///
/// C: the `u` union inside `serverSortObject`.
pub(crate) enum SortScore {
    /// Numeric sort: pre-computed float score.
    Numeric(f64),
    /// Alpha sort by-pattern: decoded string object for locale comparison.
    Alpha(Option<RedisObject>),
}

/// One element in the sort vector.
///
/// C: `serverSortObject` in `server.h`.
pub(crate) struct SortObject {
    /// The element value from the source collection.
    pub obj: RedisObject,
    pub score: SortScore,
}

// ─────────────────────────────────────────────────────────────────────────────
// Sort parameters (replaces global `server.sort_*` state)
// ─────────────────────────────────────────────────────────────────────────────

/// Carries all parameters needed by the comparison function.
///
/// In C these live on the global `server` struct so that the qsort
/// comparator (which has a fixed signature) can read them.  In Rust we pass
/// them explicitly via a closure / reference.
struct SortParams {
    desc: bool,
    alpha: bool,
    by_pattern: bool,
    store: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: is pattern the special '#' substitution marker?
// ─────────────────────────────────────────────────────────────────────────────

/// Returns `true` when `pattern` is exactly the byte string `#`.
///
/// C: `isReturnSubstPattern` (static, sort.c:49).
fn is_return_subst_pattern(pattern: &[u8]) -> bool {
    pattern == b"#"
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: createSortOperation
// ─────────────────────────────────────────────────────────────────────────────

/// Allocates a new `SortOperation`.
///
/// C: `createSortOperation` (sort.c:41-46).
fn create_sort_operation(op_type: SortOpType, pattern: RedisObject) -> SortOperation {
    SortOperation { op_type, pattern }
}

// ─────────────────────────────────────────────────────────────────────────────
// Pattern lookup
// ─────────────────────────────────────────────────────────────────────────────

/// Returns the value associated with the key whose name is derived from
/// `pattern` by substituting `*` with the bytes of `subst`.
///
/// Rules (same as C):
/// 1. If `pattern` is `#`, return `subst` itself (clone).
/// 2. Locate the first `*` in `pattern`; build the key name as
///    `prefix + subst + suffix`.
/// 3. If the suffix contains `->field`, dereference that hash field.
/// 4. Return `None` if no `*`, the key does not exist, or the type is wrong.
///
/// C: `lookupKeyByPattern` (sort.c:69-141).
///
/// TODO(port): The C version returns a `robj *` with refcount incremented by
/// 1.  In Rust we return `Option<RedisObject>` (owned clone).  Callers that
/// previously `decrRefCount`'d the return value should just drop it.
///
/// TODO(architect): `db.lookup_key_read(key)` and
/// `db.hash_type_get_value_object(obj, field)` — need canonical db/object
/// API from Phase 3.
fn lookup_key_by_pattern(
    ctx: &mut CommandContext,
    pattern: &RedisObject,
    subst: &RedisObject,
) -> Result<Option<RedisObject>, RedisError> {
    // C: sort.c:77-80 — pattern == "#" short-circuit.
    let pat_bytes: &[u8] = pattern
        .as_string_bytes()
        // TODO(port): pattern must be a string object; non-string patterns
        // are an internal contract violation; returning None is safe here.
        .unwrap_or(b"");

    if is_return_subst_pattern(pat_bytes) {
        return Ok(Some(subst.clone()));
    }

    // C: sort.c:86-88 — decode `subst` to a raw byte string.
    // PORT NOTE: `getDecodedObject` in C either returns `subst` with
    // incremented refcount (for string objects) or a freshly-created decoded
    // clone.  Here we call `as_string_bytes` which handles both cases.
    let sub_bytes: &[u8] = subst
        .as_string_bytes()
        .unwrap_or(b"");

    // C: sort.c:90-95 — find '*' in pattern.
    let star_pos = match pat_bytes.iter().position(|&b| b == b'*') {
        Some(pos) => pos,
        None => return Ok(None),
    };

    let prefix = &pat_bytes[..star_pos];
    let after_star = &pat_bytes[star_pos + 1..];

    // C: sort.c:98-103 — detect hash-dereference `->field`.
    let (postfix, field_name): (&[u8], Option<&[u8]>) =
        if let Some(arrow) = find_arrow(after_star) {
            let field = &after_star[arrow + 2..];
            if field.is_empty() {
                (after_star, None)
            } else {
                (&after_star[..arrow], Some(field))
            }
        } else {
            (after_star, None)
        };

    // C: sort.c:105-113 — build substituted key name.
    let mut key_bytes: Vec<u8> =
        Vec::with_capacity(prefix.len() + sub_bytes.len() + postfix.len());
    key_bytes.extend_from_slice(prefix);
    key_bytes.extend_from_slice(sub_bytes);
    key_bytes.extend_from_slice(postfix);

    let key = RedisString::from_bytes(&key_bytes);

    // C: sort.c:117 — lookup key in db.
    // TODO(architect): `ctx.db().lookup_key_read(&key)` — Phase 3 db API.
    let obj: Option<RedisObject> = ctx.lookup_key_read_by_bytes(&key_bytes)?;

    match (obj, field_name) {
        (None, _) => Ok(None),

        // C: sort.c:121-126 — hash dereference.
        (Some(o), Some(field)) => {
            let RedisObject::Hash(_) = &o else {
                return Ok(None);
            };
            // TODO(architect): `hash_type_get_value_object(o, field)` — Phase 3.
            let val = ctx.hash_get_field_as_object(&o, field)?;
            Ok(val)
        }

        // C: sort.c:127-132 — plain string value.
        (Some(o), None) => {
            let RedisObject::String(_) = &o else {
                return Ok(None);
            };
            Ok(Some(o))
        }
    }
}

/// Finds the byte offset of `->` within `haystack`.
fn find_arrow(haystack: &[u8]) -> Option<usize> {
    haystack.windows(2).position(|w| w == b"->")
}

// ─────────────────────────────────────────────────────────────────────────────
// Comparison
// ─────────────────────────────────────────────────────────────────────────────

/// Compares two sort-vector entries under `params`.
///
/// C: `sortCompare` (sort.c:146-193).  That function reads from the global
/// `server` struct; here the same data is in `params`.
///
/// Returns `std::cmp::Ordering`.
///
/// TODO(port): `strcoll` (locale-aware string comparison) in C's alpha,
/// non-store path is replaced by a plain lexicographic byte comparison here.
/// Locale-aware collation is not available in safe Rust without an OS binding;
/// flag for Phase B review.
fn sort_compare(a: &SortObject, b: &SortObject, params: &SortParams) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    let cmp = if !params.alpha {
        // C: sort.c:152-160 — numeric comparison.
        let sa = match &a.score {
            SortScore::Numeric(f) => *f,
            SortScore::Alpha(_) => 0.0,
        };
        let sb = match &b.score {
            SortScore::Numeric(f) => *f,
            SortScore::Alpha(_) => 0.0,
        };
        match sa.partial_cmp(&sb) {
            Some(ord) if ord != Ordering::Equal => ord,
            // C: sort.c:157-159 — tie-break lexicographically for determinism.
            _ => compare_string_objects(&a.obj, &b.obj),
        }
    } else if params.by_pattern {
        // C: sort.c:164-181 — alpha sort by external pattern value.
        let ca = match &a.score {
            SortScore::Alpha(opt) => opt.as_ref(),
            SortScore::Numeric(_) => None,
        };
        let cb = match &b.score {
            SortScore::Alpha(opt) => opt.as_ref(),
            SortScore::Numeric(_) => None,
        };
        match (ca, cb) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(oa), Some(ob)) => {
                if params.store {
                    // C: sort.c:175-176.
                    compare_string_objects(oa, ob)
                } else {
                    // C: sort.c:179-180 — strcoll path.
                    // PERF(port): strcoll() — replace with locale binding in Phase B.
                    collate_string_objects(oa, ob)
                }
            }
        }
    } else {
        // C: sort.c:183-190 — alpha, no by-pattern.
        if params.store {
            compare_string_objects(&a.obj, &b.obj)
        } else {
            collate_string_objects(&a.obj, &b.obj)
        }
    };

    if params.desc {
        cmp.reverse()
    } else {
        cmp
    }
}

/// Byte-level lexicographic comparison of two `RedisObject` string values.
///
/// C: `compareStringObjects` (object.c) — compares the raw bytes of the
/// objects' string representations.
///
/// TODO(port): `compareStringObjects` in C handles integer-encoded objects
/// by comparing the integer values numerically rather than by string.  This
/// implementation compares the byte representations, which gives the same
/// result for equal-length decimal strings but not in general.  Phase B
/// should call the real `compare_string_objects` from `redis-core`.
fn compare_string_objects(a: &RedisObject, b: &RedisObject) -> std::cmp::Ordering {
    let ba = a.as_string_bytes().unwrap_or(b"");
    let bb = b.as_string_bytes().unwrap_or(b"");
    ba.cmp(bb)
}

/// Locale-aware collation of two `RedisObject` string values.
///
/// C: `collateStringObjects` (object.c) — calls `strcoll`.
///
/// PORT NOTE: Rust has no stdlib locale/collation support.  We fall back to
/// byte-level comparison.  A proper implementation requires linking against
/// the C `strcoll` function or a Rust collation crate, which is an architect
/// decision.
///
/// TODO(architect): decide whether to call `libc::strcoll` here (requires
/// `unsafe`) or use a Rust collation crate.
fn collate_string_objects(a: &RedisObject, b: &RedisObject) -> std::cmp::Ordering {
    // PERF(port): strcoll() — locale-aware collation omitted; using byte cmp.
    compare_string_objects(a, b)
}

// ─────────────────────────────────────────────────────────────────────────────
// Score loading
// ─────────────────────────────────────────────────────────────────────────────

/// Parses a float score from a `RedisObject`.
///
/// Returns `Ok(f64)` on success, `Err` when the value cannot be converted.
///
/// C: sort.c:484-498 — inline strtod and integer-encoding fast path.
///
/// TODO(port): C checks `errno == ERANGE || errno == EINVAL` and `isnan`.
/// The Rust `f64::from_str` / `parse::<f64>` returns `Err` for those cases,
/// so this should be equivalent — but verify against the oracle.
fn parse_score_from_object(obj: &RedisObject) -> Result<f64, ()> {
    match obj {
        RedisObject::String(rs) => {
            // C: sort.c:484-489 — sdsEncodedObject path.
            let bytes = rs.as_bytes();
            // TODO(port): Rust `f64` parse requires valid UTF-8.  Redis byte
            // strings are arbitrary bytes.  Use lossy conversion for the
            // number-parsing path only (scores are expected to be ASCII).
            let s = core::str::from_utf8(bytes).map_err(|_| ())?;
            let v: f64 = s.trim().parse().map_err(|_| ())?;
            if v.is_nan() {
                return Err(());
            }
            Ok(v)
        }
        // TODO(port): integer-encoded objects — C fast-casts the pointer
        // value directly; we rely on the object exposing the integer.
        _ => Err(()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Core SORT implementation
// ─────────────────────────────────────────────────────────────────────────────

/// Generic SORT implementation shared by `sort_command` and `sort_ro_command`.
///
/// When `readonly` is `true`, the STORE option is rejected (SORT_RO).
///
/// C: `sortCommandGeneric` (sort.c:197-612).
///
/// TODO(port): This translation follows the C logic closely but relies on
/// several `CommandContext` and `RedisDb` methods that do not yet exist in
/// Phase A.  Every such call site is marked TODO(architect).
///
/// TODO(port): Cluster-mode hash-slot validation for BY and GET patterns
/// (C: sort.c:248-255, 269-275) requires `server.cluster_enabled` and
/// `patternHashSlot` / `getKeySlot`.  Skipped; mark TODO.
///
/// TODO(port): The `pqsort` partial quicksort (C: sort.c:516) is replaced by
/// a full `sort_unstable_by`.  For large result sets with a small LIMIT this
/// is O(N log N) instead of O(N + K log K).  Flag for PERF review.
pub fn sort_command_generic(ctx: &mut CommandContext, readonly: bool) -> Result<(), RedisError> {
    // ── Parse options ────────────────────────────────────────────────────────
    // C: sort.c:199-290.
    let argc = ctx.argc();

    let mut desc = false;
    let mut alpha = false;
    let mut limit_start: i64 = 0;
    let mut limit_count: i64 = -1;
    let mut dontsort = false;
    let mut sortby: Option<RedisObject> = None;
    let mut storekey: Option<RedisObject> = None;
    let mut operations: Vec<SortOperation> = Vec::new();
    let mut getop: usize = 0;

    // TODO(architect): ACL check — `user_has_full_key_access`.
    // C: sort.c:215-218.
    let user_has_full_key_access = true; // TODO(port): always true until ACL layer exists.

    let mut j = 2usize;
    while j < argc {
        let leftargs = argc - j - 1;
        let arg: &[u8] = ctx.arg_bytes(j)?;

        if arg.eq_ignore_ascii_case(b"asc") {
            desc = false;
        } else if arg.eq_ignore_ascii_case(b"desc") {
            desc = true;
        } else if arg.eq_ignore_ascii_case(b"alpha") {
            alpha = true;
        } else if arg.eq_ignore_ascii_case(b"limit") && leftargs >= 2 {
            // C: sort.c:228-233.
            limit_start = ctx.arg_parse_i64(j + 1)?;
            limit_count = ctx.arg_parse_i64(j + 2)?;
            j += 2;
        } else if !readonly && arg.eq_ignore_ascii_case(b"store") && leftargs >= 1 {
            // C: sort.c:235-237.
            storekey = Some(ctx.arg_object(j + 1)?.clone());
            j += 1;
        } else if arg.eq_ignore_ascii_case(b"by") && leftargs >= 1 {
            // C: sort.c:238-263.
            let by_arg = ctx.arg_bytes(j + 1)?;
            if !by_arg.contains(&b'*') {
                // Constant BY pattern — skip sorting entirely.
                dontsort = true;
            } else {
                // TODO(port): cluster-mode slot check omitted.
                // C: sort.c:248-255.
                if !user_has_full_key_access {
                    return Err(RedisError::runtime(
                        b"BY option of SORT denied due to insufficient ACL permissions.",
                    ));
                }
            }
            sortby = Some(ctx.arg_object(j + 1)?.clone());
            j += 1;
        } else if arg.eq_ignore_ascii_case(b"get") && leftargs >= 1 {
            // C: sort.c:264-282.
            let get_arg = ctx.arg_bytes(j + 1)?;
            // TODO(port): cluster-mode slot check omitted.
            // C: sort.c:268-274.
            if !is_return_subst_pattern(get_arg) && !user_has_full_key_access {
                return Err(RedisError::runtime(
                    b"GET option of SORT denied due to insufficient ACL permissions.",
                ));
            }
            let pattern_obj = ctx.arg_object(j + 1)?.clone();
            operations.push(create_sort_operation(SortOpType::Get, pattern_obj));
            getop += 1;
            j += 1;
        } else {
            // C: sort.c:285-287 — shared.syntaxerr.
            return Err(RedisError::syntax(b"syntax error"));
        }
        j += 1;
    }

    // ── Lookup the key to sort ───────────────────────────────────────────────
    // C: sort.c:299-313.

    // TODO(architect): `ctx.lookup_key_read(key)` — Phase 3 db API.
    let key_bytes = ctx.arg_bytes(1)?.to_owned();
    let sortval_opt: Option<RedisObject> = ctx.lookup_key_read_by_bytes(&key_bytes)?;

    match &sortval_opt {
        Some(sv)
            if !matches!(
                sv,
                RedisObject::List(_) | RedisObject::Set(_) | RedisObject::ZSet(_)
            ) =>
        {
            return Err(RedisError::wrong_type());
        }
        _ => {}
    }

    // C: sort.c:309-313 — if key is absent, treat as empty list.
    // PORT NOTE: We represent an absent key as an empty Vec<RedisObject>
    // for the list path; there is no "empty quicklist object" in Rust.
    let is_absent = sortval_opt.is_none();

    // ── Compute vector length ────────────────────────────────────────────────
    // C: sort.c:328-336.
    let vectorlen_base: i64 = match &sortval_opt {
        None => 0,
        Some(RedisObject::List(lst)) => lst.len() as i64,
        Some(RedisObject::Set(st)) => st.len() as i64,
        Some(RedisObject::ZSet(zs)) => zs.len() as i64,
        Some(_) => {
            // TODO(architect): handle unknown object type.
            // C: sort.c:335 — serverPanic.
            // TODO(architect): is panic correct here?
            0
        }
    };

    // ── dontsort override for Set in scripting/store context ─────────────────
    // C: sort.c:319-325.
    let (mut dontsort, mut alpha, mut sortby) = (dontsort, alpha, sortby);
    if dontsort
        && matches!(sortval_opt, Some(RedisObject::Set(_)))
        && (storekey.is_some() || ctx.is_script_context())
    {
        dontsort = false;
        alpha = true;
        sortby = None;
    }

    // ── LIMIT sanity check ───────────────────────────────────────────────────
    // C: sort.c:340-347.
    let vlen = vectorlen_base;
    let start = (limit_start.max(0)).min(vlen);
    let limit_count = limit_count.max(-1).min(vlen);
    let mut end = if limit_count < 0 {
        vlen - 1
    } else {
        start + limit_count - 1
    };

    let (mut start, mut end) = if start >= vlen {
        (vlen - 1, vlen - 2)
    } else {
        (start, end)
    };
    if end >= vlen {
        end = vlen - 1;
    }

    // C: sort.c:359-361 — LIMIT optimisation for sorted set / list + dontsort.
    let mut vectorlen = vlen;
    if (matches!(sortval_opt, Some(RedisObject::ZSet(_)))
        || matches!(sortval_opt, Some(RedisObject::List(_))))
        && dontsort
        && (start != 0 || end != vlen - 1)
    {
        vectorlen = end - start + 1;
    }

    // ── Build sort vector ────────────────────────────────────────────────────
    // C: sort.c:363-465.
    //
    // TODO(port): All collection iteration below depends on the runtime
    // representation of `RedisObject::List`, `Set`, and `ZSet` variants.
    // These are stubbed with empty iterators until the data-structure crates
    // are wired in Phase 3+.

    let mut vector: Vec<SortObject> = Vec::with_capacity(vectorlen as usize);

    match &sortval_opt {
        None => {
            // Absent key — empty vector; nothing to do.
        }

        Some(RedisObject::List(lst)) if dontsort => {
            // C: sort.c:367-390 — list + dontsort; iterate in output order.
            // PORT NOTE: DESC reversal and LIMIT slicing are handled by
            // choosing the right sub-slice of the list.
            let items: Vec<RedisObject> = if desc {
                lst.iter()
                    .rev()
                    .skip(start as usize)
                    .take(vectorlen as usize)
                    .cloned()
                    .collect()
            } else {
                lst.iter()
                    .skip(start as usize)
                    .take(vectorlen as usize)
                    .cloned()
                    .collect()
            };
            for obj in items {
                vector.push(SortObject {
                    obj,
                    score: SortScore::Numeric(0.0),
                });
            }
            // C: sort.c:388-389 — fix start/end after optimisation.
            end -= start;
            start = 0;
        }

        Some(RedisObject::List(lst)) => {
            // C: sort.c:391-399 — plain list iteration.
            for obj in lst.iter().cloned() {
                vector.push(SortObject {
                    obj,
                    score: SortScore::Numeric(0.0),
                });
            }
        }

        Some(RedisObject::Set(st)) => {
            // C: sort.c:401-409 — set iteration.
            for obj in st.iter().cloned() {
                vector.push(SortObject {
                    obj,
                    score: SortScore::Numeric(0.0),
                });
            }
        }

        Some(RedisObject::ZSet(zs)) if dontsort => {
            // C: sort.c:411-447 — sorted set + dontsort; respect internal order.
            // TODO(port): Skiplist traversal with start/rank offset.
            // The C path calls `zslGetElementByRank` and walks forward/backward.
            // We fall back to collecting all elements sorted by score then
            // applying the direction, flagging for Phase B.
            let mut items: Vec<(f64, RedisObject)> = zs
                .iter()
                .map(|(score, obj)| (*score, obj.clone()))
                .collect();
            items.sort_by(|(sa, _), (sb, _)| sa.partial_cmp(sb).unwrap_or(std::cmp::Ordering::Equal));
            if desc {
                items.reverse();
            }
            for (_, obj) in items
                .into_iter()
                .skip(start as usize)
                .take(vectorlen as usize)
            {
                vector.push(SortObject {
                    obj,
                    score: SortScore::Numeric(0.0),
                });
            }
            // C: sort.c:447-448 — fix start/end.
            end -= start;
            start = 0;
        }

        Some(RedisObject::ZSet(zs)) => {
            // C: sort.c:449-464 — sorted set without dontsort: iterate ht.
            for (_, obj) in zs.iter() {
                vector.push(SortObject {
                    obj: obj.clone(),
                    score: SortScore::Numeric(0.0),
                });
            }
        }

        Some(_) => {
            // TODO(port): unreachable after the type guard above; left for
            // completeness.
        }
    }

    debug_assert_eq!(vector.len(), vectorlen as usize);

    // ── Load scores ──────────────────────────────────────────────────────────
    // C: sort.c:469-519.
    let mut int_conversion_error = false;

    if !dontsort {
        for entry in vector.iter_mut() {
            let byval: Option<RedisObject> = if let Some(by) = &sortby {
                // C: sort.c:473-475 — lookup weight key.
                lookup_key_by_pattern(ctx, by, &entry.obj)?
            } else {
                // C: sort.c:477-479 — use object itself.
                Some(entry.obj.clone())
            };

            let byval = match byval {
                None => continue, // C: sort.c:475 — `if (!byval) continue`.
                Some(v) => v,
            };

            if alpha {
                if sortby.is_some() {
                    // C: sort.c:482-483 — store decoded object for comparison.
                    // PORT NOTE: `getDecodedObject` — clone is equivalent for
                    // string objects; integer-encoded objects need decoding.
                    entry.score = SortScore::Alpha(Some(byval));
                }
            } else {
                // C: sort.c:484-498 — numeric conversion.
                match parse_score_from_object(&byval) {
                    Ok(f) => {
                        entry.score = SortScore::Numeric(f);
                    }
                    Err(_) => {
                        int_conversion_error = true;
                    }
                }
            }
        }

        // C: sort.c:508-519 — sort the vector.
        let params = SortParams {
            desc,
            alpha,
            by_pattern: sortby.is_some(),
            store: storekey.is_some(),
        };

        if vectorlen > 0 {
            // PERF(port): pqsort (partial quicksort for LIMIT) — C: sort.c:515-518.
            // Using full sort; replace with partial sort in Phase B for perf.
            vector.sort_unstable_by(|a, b| sort_compare(a, b, &params));
        }
    }

    // ── Compute output length ────────────────────────────────────────────────
    // C: sort.c:524.
    let range_len = if end >= start { (end - start + 1) as usize } else { 0 };
    let outputlen: usize = if getop > 0 {
        getop * range_len
    } else {
        range_len
    };

    // ── Send reply or store ──────────────────────────────────────────────────
    if int_conversion_error {
        // C: sort.c:525-526.
        return Err(RedisError::runtime(
            b"One or more scores can't be converted into double",
        ));
    }

    if storekey.is_none() {
        // C: sort.c:527-552 — send array reply to client.
        ctx.reply_array_header(outputlen as i64)?;

        for idx in start..=end {
            let idx = idx as usize;
            if getop == 0 {
                // C: sort.c:534.
                ctx.reply_bulk_object(&vector[idx].obj)?;
            }
            for op in &operations {
                // C: sort.c:536-548.
                let val = lookup_key_by_pattern(ctx, &op.pattern, &vector[idx].obj)?;
                if op.op_type == SortOpType::Get {
                    match val {
                        None => ctx.reply_null()?,
                        Some(v) => ctx.reply_bulk_object(&v)?,
                    }
                } else {
                    // C: sort.c:548-550 — "Always fails".
                    debug_assert!(false, "only SORT_OP_GET is supported");
                }
            }
        }
    } else {
        // C: sort.c:553-601 — STORE path.
        // TODO(architect): `ctx.db_mut().set_key(storekey, list_obj)` —
        // Phase 3 db write API.

        let mut result_list: Vec<RedisObject> = Vec::with_capacity(outputlen);

        for idx in start..=end {
            let idx = idx as usize;
            if getop == 0 {
                // C: sort.c:564.
                result_list.push(vector[idx].obj.clone());
            } else {
                // C: sort.c:566-584.
                for op in &operations {
                    let val = lookup_key_by_pattern(ctx, &op.pattern, &vector[idx].obj)?;
                    if op.op_type == SortOpType::Get {
                        let v = val.unwrap_or_else(|| {
                            // C: sort.c:572 — empty string placeholder.
                            RedisObject::String(RedisString::from_bytes(b""))
                        });
                        result_list.push(v);
                    } else {
                        debug_assert!(false, "only SORT_OP_GET is supported");
                    }
                }
            }
        }

        if !result_list.is_empty() {
            // C: sort.c:587-594.
            let store_key_obj = storekey.as_ref();
            // TODO(architect): `ctx.db_mut().set_key(store_key_obj, result_list)` — Phase 3.
            // TODO(architect): `ctx.notify_keyspace_event(NOTIFY_LIST, "sortstore", ...)`.
            // TODO(architect): `ctx.server_mut().dirty += outputlen`.
        } else if {
            // C: sort.c:594-598 — delete storekey if output is empty.
            // TODO(architect): `ctx.db_mut().delete(storekey)` — Phase 3.
            // TODO(architect): `ctx.signal_modified_key(storekey)`.
            // TODO(architect): `ctx.notify_keyspace_event(NOTIFY_GENERIC, "del", ...)`.
            // TODO(architect): `ctx.server_mut().dirty += 1`.
            false // placeholder; real delete result used in C.
        } {
        }

        ctx.reply_integer(outputlen as i64)?;
    }

    // Cleanup is implicit: `vector`, `operations`, `sortval_opt` are dropped.
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Public command entry points
// ─────────────────────────────────────────────────────────────────────────────

/// SORT command entry point (read-write).
///
/// C: `sortCommand` (sort.c:619-621).
pub fn sort_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    sort_command_generic(ctx, false)
}

/// SORT_RO command entry point (read-only; STORE option is refused).
///
/// C: `sortroCommand` (sort.c:614-617).
pub fn sort_ro_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    sort_command_generic(ctx, true)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/sort.c  (622 lines, 5 functions)
//   target_crate:  redis-commands
//   confidence:    medium
//   todos:         25
//   port_notes:    4
//   unsafe_blocks: 0
//   notes:         Logic faithfully ported; all missing db/collection APIs
//                  marked TODO(architect).  Cluster slot checks, partial-sort
//                  optimisation (pqsort→full sort), locale collation
//                  (strcoll→byte cmp), and integer-encoded object fast-paths
//                  need Phase B attention.  All rustc errors are expected
//                  name-resolution failures (E0282, E0432, E0433).
// ──────────────────────────────────────────────────────────────────────────
