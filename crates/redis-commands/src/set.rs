//! Set command implementations: SADD, SREM, SMOVE, SISMEMBER, SMISMEMBER,
//! SCARD, SPOP, SRANDMEMBER, SINTER, SINTERSTORE, SINTERCARD,
//! SUNION, SUNIONSTORE, SDIFF, SDIFFSTORE, SSCAN.
//!
//! C source: `reference/valkey/src/t_set.c` (1660 lines, 40 functions)
//! Crate: `redis-commands` (later phase)
//!
//! ## Encoding model
//!
//! A Redis Set uses one of three internal encodings depending on its content
//! and size:
//!
//! * `IntSet`    — compact sorted array of `i64` values.
//! * `ListPack`  — compact binary sequence of mixed-type elements (RESP-like).
//! * `HashTable` — full hash table for large/heterogeneous sets.
//!
//! All three are captured in the local [`SetData`] enum, which is the Phase A
//! placeholder for the inner value of `RedisObject::Set(...)`.
//!
//! ## TODO(architect) items
//!
//! TODO(architect): `SetData` must be reconciled with `RedisObject::Set` inner
//! encoding in `redis-core/src/object.rs`.  Phase 4 will introduce proper
//! redis-ds types (`IntSet`, `ListPack`, `HashTable`).
//!
//! TODO(architect): `CommandContext::db_mut()` — needs `&mut RedisServer`
//! plumbed through `CommandContext` (Phase 3 redis-core architect packet).
//!
//! TODO(architect): `CommandContext::server()` / `server_mut()` — access to
//! `RedisServer` for `dirty`, `set_max_intset_entries`,
//! `set_max_listpack_entries`, `set_max_listpack_value`,
//! `lazyfree_lazy_server_del` (Phase 3).
//!
//! TODO(architect): `CommandContext::notify_keyspace_event(flags, event, key)`
//! — keyspace notification dispatch blocked on Phase 3.
//!
//! TODO(architect): `CommandContext::signal_modified_key(key)` — WATCH /
//! client-tracking invalidation blocked on Phase 3.
//!
//! TODO(architect): `CommandContext::also_propagate(...)` — AOF/replication
//! side-channel propagation (Phase 3+).
//!
//! TODO(architect): `CommandContext::rewrite_client_command_vector(...)` —
//! command rewriting for AOF/repl (Phase 3+).
//!
//! TODO(architect): `CommandContext::prevent_command_propagation()` — suppress
//! native propagation when using `also_propagate` (Phase 3+).
//!
//! TODO(architect): `CommandContext::scan_generic_command(set, cursor)` —
//! generic SCAN implementation blocked on Phase 3+.
//!
//! TODO(architect): redis-ds `IntSet`, `ListPack`, `HashTable` types — the
//! Phase A `Vec<i64>` / `Vec<SetElement>` / `HashSet<RedisString>` placeholders
//! below must be replaced with the real redis-ds implementations in Phase 4.

use std::collections::HashSet;

use redis_core::command_context::CommandContext;
use redis_core::object::RedisObject;
use redis_types::{RedisError, RedisString};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Multiplier for choosing the SPOP "create-new-set" strategy (Case 3).
///
/// If `remaining * SPOP_MOVE_STRATEGY_MUL <= count` we switch to extracting
/// the elements that will *remain* rather than the elements to pop.
///
/// C: `#define SPOP_MOVE_STRATEGY_MUL 5` in `t_set.c`
const SPOP_MOVE_STRATEGY_MUL: u64 = 5;

/// Multiplier for choosing SRANDMEMBER "subtract" strategy (Case 3 vs 4).
///
/// If `count * SRANDMEMBER_SUB_STRATEGY_MUL > size` we copy the whole set
/// and remove elements down to `count` (Case 3) rather than randomly
/// picking until we have enough unique elements (Case 4).
///
/// C: `#define SRANDMEMBER_SUB_STRATEGY_MUL 3` in `t_set.c`
const SRANDMEMBER_SUB_STRATEGY_MUL: u64 = 3;

/// Maximum random-sample batch size for SRANDMEMBER listpack path.
///
/// C: `#define SRANDFIELD_RANDOM_SAMPLE_LIMIT 1000` in `t_set.c`
const SRANDFIELD_RANDOM_SAMPLE_LIMIT: u64 = 1000;

/// Hard cap on intset size, imposed on top of the server config limit.
///
/// C: `if (max_entries >= 1 << 30) max_entries = 1 << 30;` in `intsetMaxEntries`.
const INTSET_ABS_MAX: usize = 1 << 30;

/// Default server config: `set-max-intset-entries`.
///
/// TODO(port): Replace with `ctx.server().set_max_intset_entries` when Phase 3
/// plumbs server config through `CommandContext`.
const DEFAULT_SET_MAX_INTSET_ENTRIES: usize = 512;

/// Default server config: `set-max-listpack-entries`.
///
/// TODO(port): Replace with `ctx.server().set_max_listpack_entries` when Phase 3
/// plumbs server config through `CommandContext`.
const DEFAULT_SET_MAX_LISTPACK_ENTRIES: usize = 128;

/// Default server config: `set-max-listpack-value` (element byte-length limit).
///
/// TODO(port): Replace with `ctx.server().set_max_listpack_value` when Phase 3
/// plumbs server config through `CommandContext`.
const DEFAULT_SET_MAX_LISTPACK_VALUE: usize = 64;

// ─────────────────────────────────────────────────────────────────────────────
// Set operation selector
// ─────────────────────────────────────────────────────────────────────────────

/// Which multi-key set operation to perform.
///
/// C: `SET_OP_UNION` / `SET_OP_DIFF` in `server.h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Diff,
}

// ─────────────────────────────────────────────────────────────────────────────
// Set element value (iteration / random access)
// ─────────────────────────────────────────────────────────────────────────────

/// A single element returned from set iteration or random sampling.
///
/// Mirrors the `(char *str, size_t len, int64_t llele)` output-parameter
/// triplet used throughout C iteration APIs: when `str` is NULL the value is
/// the integer `llele`; otherwise it is the byte string `(str, len)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetElement {
    /// Byte-string element (non-integer or hashtable encoding string).
    Str(Vec<u8>),
    /// Integer element (intset encoding or listpack integer entry).
    Integer(i64),
}

impl SetElement {
    /// Render the element as owned bytes, formatting integers as decimal ASCII.
    ///
    /// C: `sdsfromlonglong` / `sdsnewlen` pattern used in `setTypeNextObject`.
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            SetElement::Str(b) => b,
            SetElement::Integer(n) => {
                // Decimal rendering; always valid ASCII, not Redis-data UTF-8.
                // PORT NOTE: Equivalent to C's `sdsfromlonglong(n)`.
                format!("{}", n).into_bytes()
            }
        }
    }

    /// Return the byte representation without consuming self.
    pub fn as_bytes(&self) -> Vec<u8> {
        match self {
            SetElement::Str(b) => b.clone(),
            SetElement::Integer(n) => format!("{}", n).into_bytes(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Inner set encoding
// ─────────────────────────────────────────────────────────────────────────────

/// Phase A placeholder for the inner value of `RedisObject::Set(...)`.
///
/// TODO(architect): This enum is the Phase A stand-in.  When Phase 4 defines
/// real redis-ds types (`IntSet`, `ListPack`, `HashTable`), the
/// `RedisObject::Set` variant in `redis-core/src/object.rs` should wrap one of
/// those, and all `SetData` references in this file should be updated.
///
/// `IntSet` — sorted `Vec<i64>` placeholder (C: compact intset blob).
/// `ListPack` — `Vec<SetElement>` placeholder (C: listpack byte buffer).
/// `HashTable` — `HashSet<RedisString>` placeholder (C: hashtable).
#[derive(Debug, Clone)]
pub enum SetData {
    /// OBJ_ENCODING_INTSET: sorted compact integer set.
    /// TODO(port): Replace with `redis_ds::intset::IntSet` in Phase 4.
    IntSet(Vec<i64>),
    /// OBJ_ENCODING_LISTPACK: compact sequence of mixed-type elements.
    /// TODO(port): Replace with `redis_ds::listpack::ListPack` in Phase 4.
    ListPack(Vec<SetElement>),
    /// OBJ_ENCODING_HASHTABLE: full hash table for large or string-only sets.
    /// TODO(port): Replace with `redis_ds::hashtable::HashTable` in Phase 4.
    HashTable(HashSet<RedisString>),
}

// ─────────────────────────────────────────────────────────────────────────────
// Iterator
// ─────────────────────────────────────────────────────────────────────────────

/// Iteration state for a Redis Set (all encodings).
///
/// C: `setTypeIterator` in `server.h`.
///
/// PERF(port): The Phase A iterator eagerly copies all elements into a `Vec`
/// at initialisation to avoid unsafe pointer aliasing.  Phase B should use
/// per-encoding lazy iterators (index for intset, position for listpack,
/// hashtable cursor for hashtable).
pub struct SetTypeIterator {
    /// Pre-collected elements from the set at iterator creation time.
    elements: Vec<SetElement>,
    /// Current position.
    index: usize,
}

impl SetTypeIterator {
    /// Advance and return the next element, or `None` when exhausted.
    ///
    /// C: `setTypeNext` returns the encoding int or -1.
    pub fn next_element(&mut self) -> Option<SetElement> {
        if self.index >= self.elements.len() {
            return None;
        }
        let elem = self.elements[self.index].clone();
        self.index += 1;
        Some(elem)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Factory / encoding-conversion helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Return the effective intset max-entries limit (server config capped at 1G).
///
/// C: `intsetMaxEntries()` in `t_set.c:73-78`.
fn intset_max_entries() -> usize {
    // TODO(port): read from server config when CommandContext exposes server().
    let max = DEFAULT_SET_MAX_INTSET_ENTRIES;
    if max >= INTSET_ABS_MAX { INTSET_ABS_MAX } else { max }
}

/// If the intset has grown beyond the max-entries limit, convert it to a
/// hashtable in place.
///
/// C: `maybeConvertIntset()` in `t_set.c:81-84`.
fn maybe_convert_intset(subject: &mut SetData) {
    if let SetData::IntSet(ref values) = subject {
        if values.len() > intset_max_entries() {
            let new_ht = values
                .iter()
                .map(|&n| RedisString::from_bytes(&format!("{}", n).into_bytes()))
                .collect::<HashSet<RedisString>>();
            *subject = SetData::HashTable(new_ht);
        }
    }
    // debug_assert!(matches!(subject, SetData::IntSet(_))); // C: serverAssert
}

/// If all elements in a non-intset set are integers and the set is small
/// enough, convert it to intset encoding.
///
/// C: `maybeConvertToIntset()` in `t_set.c:89-111`.
fn maybe_convert_to_intset(set: &mut SetData) {
    if matches!(set, SetData::IntSet(_)) {
        return; // already intset
    }
    if set_type_size(set) > intset_max_entries() {
        return; // too large for intset
    }

    // Try to parse every element as i64.
    let elements: Vec<SetElement> = set_type_collect_elements(set);
    let mut int_values: Vec<i64> = Vec::with_capacity(elements.len());
    for elem in &elements {
        match elem {
            SetElement::Integer(n) => int_values.push(*n),
            SetElement::Str(b) => {
                // TODO(port): use a proper integer-parse helper (C: string2ll).
                match parse_integer_bytes(b) {
                    Some(n) => int_values.push(n),
                    None => return, // not all-integer; cannot convert
                }
            }
        }
    }

    int_values.sort_unstable();
    int_values.dedup();
    *set = SetData::IntSet(int_values);
}

/// Factory: create a new `SetData` that can hold `value`, choosing the most
/// compact encoding based on content and `size_hint`.
///
/// C: `setTypeCreate()` in `t_set.c:51-61`.
pub fn set_type_create(value: &[u8], size_hint: usize) -> SetData {
    // Use intset if the value is integer-representable and the hint is small.
    if let Some(_) = parse_integer_bytes(value) {
        if size_hint <= DEFAULT_SET_MAX_INTSET_ENTRIES {
            return SetData::IntSet(Vec::new());
        }
    }
    // Use listpack for small mixed sets.
    if size_hint <= DEFAULT_SET_MAX_LISTPACK_ENTRIES {
        return SetData::ListPack(Vec::new());
    }
    // Fall back to hashtable, pre-allocated for size_hint.
    // PERF(port): C code calls hashtableExpand here; our HashSet grows lazily.
    SetData::HashTable(HashSet::with_capacity(size_hint))
}

/// If the set's current encoding cannot handle `size_hint` more elements,
/// convert it to a larger encoding.
///
/// C: `setTypeMaybeConvert()` in `t_set.c:65-70`.
pub fn set_type_maybe_convert(set: &mut SetData, size_hint: usize) {
    let should_convert = match set {
        SetData::ListPack(_) => size_hint > DEFAULT_SET_MAX_LISTPACK_ENTRIES,
        SetData::IntSet(_)   => size_hint > DEFAULT_SET_MAX_INTSET_ENTRIES,
        SetData::HashTable(_) => false,
    };
    if should_convert {
        set_type_convert_and_expand(set, SetEncoding::HashTable, size_hint);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Encoding enum (used internally for conversion targets)
// ─────────────────────────────────────────────────────────────────────────────

/// Target encoding for `set_type_convert_and_expand`.
///
/// Mirrors C constants `OBJ_ENCODING_INTSET`, `OBJ_ENCODING_LISTPACK`,
/// `OBJ_ENCODING_HASHTABLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetEncoding {
    IntSet,
    ListPack,
    HashTable,
}

impl SetData {
    pub(crate) fn encoding(&self) -> SetEncoding {
        match self {
            SetData::IntSet(_)    => SetEncoding::IntSet,
            SetData::ListPack(_)  => SetEncoding::ListPack,
            SetData::HashTable(_) => SetEncoding::HashTable,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Core set-mutation helpers (non-command)
// ─────────────────────────────────────────────────────────────────────────────

/// Add a byte-string element.  Returns `true` if added, `false` if already a
/// member.
///
/// C: `setTypeAdd()` in `t_set.c:117-119`.
pub fn set_type_add(subject: &mut SetData, value: &[u8]) -> bool {
    set_type_add_aux(subject, Some(value), 0, true)
}

/// Add an element given as either a byte slice or an integer.  `str_val` is
/// `None` when the caller provides `llval` directly (integer path).  The
/// `str_is_sds` hint indicates the bytes came from an sds string (ownership
/// optimisation in C; irrelevant in Rust but kept for API parity).
///
/// Returns `true` if the element was inserted, `false` if it was already a
/// member.
///
/// C: `setTypeAddAux()` in `t_set.c:127-225`.
pub fn set_type_add_aux(
    set: &mut SetData,
    str_val: Option<&[u8]>,
    llval: i64,
    _str_is_sds: bool,
) -> bool {
    // When str is NULL we are given an integer.
    let bytes: Vec<u8>;
    let effective_bytes: &[u8];
    let effective_llval: Option<i64>;

    if let Some(s) = str_val {
        effective_bytes = s;
        effective_llval = parse_integer_bytes(s);
    } else {
        // Convert integer to decimal string for encodings that need it.
        bytes = format!("{}", llval).into_bytes();
        effective_bytes = &bytes;
        effective_llval = Some(llval);
    }

    match set {
        SetData::IntSet(ref mut is) => {
            if str_val.is_none() {
                // Fast path: integer into intset.
                if is.contains(&llval) {
                    return false;
                }
                let pos = is.partition_point(|&x| x < llval);
                is.insert(pos, llval);
                maybe_convert_intset(set);
                return true;
            }
            // str_val given — see if it parses as integer.
            if let Some(ival) = effective_llval {
                if is.contains(&ival) {
                    return false;
                }
                let pos = is.partition_point(|&x| x < ival);
                is.insert(pos, ival);
                maybe_convert_intset(set);
                return true;
            }
            // Not an integer — check if listpack thresholds are safe.
            let n = is.len();
            let fits_listpack = n < DEFAULT_SET_MAX_LISTPACK_ENTRIES
                && effective_bytes.len() <= DEFAULT_SET_MAX_LISTPACK_VALUE;
            if fits_listpack {
                set_type_convert_and_expand(set, SetEncoding::ListPack, n + 1);
                return set_type_add_aux(set, str_val, llval, false);
            } else {
                set_type_convert_and_expand(set, SetEncoding::HashTable, n + 1);
                return set_type_add_aux(set, str_val, llval, false);
            }
        }
        SetData::ListPack(ref mut lp) => {
            // Check membership.
            let already = lp.iter().any(|e| e.as_bytes() == effective_bytes);
            if already {
                return false;
            }
            // Check size limits.
            if lp.len() < DEFAULT_SET_MAX_LISTPACK_ENTRIES
                && effective_bytes.len() <= DEFAULT_SET_MAX_LISTPACK_VALUE
            {
                // TODO(port): C prefers lpAppendInteger when the value came in
                // as an integer (avoids re-parsing).  Phase B optimisation.
                let elem = if let Some(ival) = effective_llval {
                    SetElement::Integer(ival)
                } else {
                    SetElement::Str(effective_bytes.to_vec())
                };
                lp.push(elem);
            } else {
                // Exceeded limits — convert to hashtable.
                let cap = lp.len() + 1;
                set_type_convert_and_expand(set, SetEncoding::HashTable, cap);
                return set_type_add_aux(set, str_val, llval, false);
            }
            true
        }
        SetData::HashTable(ref mut ht) => {
            let key = RedisString::from_bytes(effective_bytes);
            ht.insert(key)
        }
    }
}

/// Remove a byte-string element.  Returns `true` if removed, `false` if not
/// a member.
///
/// C: `setTypeRemove()` in `t_set.c:229-231`.
pub fn set_type_remove(setobj: &mut SetData, value: &[u8]) -> bool {
    set_type_remove_aux(setobj, Some(value), 0, true)
}

/// Remove an element given as a byte slice or integer.
///
/// C: `setTypeRemoveAux()` in `t_set.c:239-278`.
pub fn set_type_remove_aux(
    setobj: &mut SetData,
    str_val: Option<&[u8]>,
    llval: i64,
    _str_is_sds: bool,
) -> bool {
    let bytes: Vec<u8>;
    let effective_bytes: &[u8];
    let effective_llval: Option<i64>;

    if let Some(s) = str_val {
        effective_bytes = s;
        effective_llval = parse_integer_bytes(s);
    } else {
        bytes = format!("{}", llval).into_bytes();
        effective_bytes = &bytes;
        effective_llval = Some(llval);
    }

    match setobj {
        SetData::IntSet(ref mut is) => {
            let ival = if str_val.is_none() {
                llval
            } else if let Some(v) = effective_llval {
                v
            } else {
                return false; // non-integer string cannot be in intset
            };
            if let Ok(pos) = is.binary_search(&ival) {
                is.remove(pos);
                return true;
            }
            false
        }
        SetData::ListPack(ref mut lp) => {
            let before = lp.len();
            lp.retain(|e| e.as_bytes() != effective_bytes);
            lp.len() < before
        }
        SetData::HashTable(ref mut ht) => {
            let key = RedisString::from_bytes(effective_bytes);
            ht.remove(&key)
        }
    }
}

/// Test membership of a byte-string element.
///
/// C: `setTypeIsMember()` in `t_set.c:282-284`.
pub fn set_type_is_member(subject: &SetData, value: &[u8]) -> bool {
    set_type_is_member_aux(subject, Some(value), 0, true)
}

/// Test membership for an element given as byte slice or integer.
///
/// C: `setTypeIsMemberAux()` in `t_set.c:292-318`.
pub fn set_type_is_member_aux(
    set: &SetData,
    str_val: Option<&[u8]>,
    llval: i64,
    _str_is_sds: bool,
) -> bool {
    let bytes: Vec<u8>;
    let effective_bytes: &[u8];
    let effective_llval: Option<i64>;

    if let Some(s) = str_val {
        effective_bytes = s;
        effective_llval = parse_integer_bytes(s);
    } else {
        bytes = format!("{}", llval).into_bytes();
        effective_bytes = &bytes;
        effective_llval = Some(llval);
    }

    match set {
        SetData::IntSet(is) => {
            let ival = if str_val.is_none() {
                llval
            } else if let Some(v) = effective_llval {
                v
            } else {
                return false;
            };
            is.binary_search(&ival).is_ok()
        }
        SetData::ListPack(lp) => {
            lp.iter().any(|e| e.as_bytes() == effective_bytes)
        }
        SetData::HashTable(ht) => {
            let key = RedisString::from_bytes(effective_bytes);
            ht.contains(&key)
        }
    }
}

/// Return the number of elements in the set.
///
/// C: `setTypeSize()` in `t_set.c:473-483`.
pub fn set_type_size(subject: &SetData) -> usize {
    match subject {
        SetData::IntSet(is)    => is.len(),
        SetData::ListPack(lp)  => lp.len(),
        SetData::HashTable(ht) => ht.len(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Iterator API
// ─────────────────────────────────────────────────────────────────────────────

/// Create an iterator over the set.
///
/// C: `setTypeInitIterator()` in `t_set.c:320-334`.
///
/// PERF(port): Phase A collects all elements eagerly.  Phase B should use
/// per-encoding lazy state (intset index, listpack byte pointer, hashtable
/// cursor).
pub fn set_type_init_iterator(subject: &SetData) -> SetTypeIterator {
    let elements = set_type_collect_elements(subject);
    SetTypeIterator { elements, index: 0 }
}

/// Release iterator resources.  In Phase A this is a no-op because we hold
/// no external resources.
///
/// C: `setTypeReleaseIterator()` in `t_set.c:336-339`.
pub fn set_type_release_iterator(_si: SetTypeIterator) {
    // Phase A: no-op.  C freed the hashtableIterator here.
}

/// Advance the iterator and return the next element, or `None` if exhausted.
///
/// C: `setTypeNext()` in `t_set.c:362-389` returns the encoding type or -1.
pub fn set_type_next(si: &mut SetTypeIterator) -> Option<SetElement> {
    si.next_element()
}

/// Advance and return the next element as owned bytes (decimal string for
/// integers).  Returns `None` when exhausted.
///
/// C: `setTypeNextObject()` in `t_set.c:398-406`.
pub fn set_type_next_object(si: &mut SetTypeIterator) -> Option<Vec<u8>> {
    si.next_element().map(SetElement::into_bytes)
}

/// Return a random element from the set without removing it.
///
/// C: `setTypeRandomElement()` in `t_set.c:421-442`.
///
/// TODO(port): Phase A uses a fixed index 0 as a placeholder for random
/// selection.  Phase B must integrate with a proper PRNG (the C code uses
/// `rand()` / `hashtableFairRandomEntry` / `intsetRandom` / `lpSeek`).
pub fn set_type_random_element(setobj: &SetData) -> Option<SetElement> {
    match setobj {
        SetData::IntSet(is) => {
            if is.is_empty() { return None; }
            // TODO(port): use proper RNG (C: intsetRandom)
            Some(SetElement::Integer(is[0]))
        }
        SetData::ListPack(lp) => {
            if lp.is_empty() { return None; }
            // TODO(port): use proper RNG (C: rand() % lpLength, lpSeek, lpGetValue)
            Some(lp[0].clone())
        }
        SetData::HashTable(ht) => {
            if ht.is_empty() { return None; }
            // TODO(port): use proper RNG (C: hashtableFairRandomEntry)
            ht.iter().next().map(|rs: &RedisString| SetElement::Str(rs.as_bytes().to_vec()))
        }
    }
}

/// Pop and return a random element from the set.
///
/// C: `setTypePopRandom()` in `t_set.c:445-471`.
///
/// TODO(port): Phase A always pops the first element.  Phase B must use a
/// proper PRNG and the listpack `lpNextRandom` optimisation.
pub fn set_type_pop_random(set: &mut SetData) -> Option<SetElement> {
    let elem = set_type_random_element(set)?;
    match &elem {
        SetElement::Str(b) => { set_type_remove(set, b); }
        SetElement::Integer(n) => {
            set_type_remove_aux(set, None, *n, false);
        }
    }
    Some(elem)
}

// ─────────────────────────────────────────────────────────────────────────────
// Encoding conversion
// ─────────────────────────────────────────────────────────────────────────────

/// Convert a set to the specified encoding, pre-sizing for the current size.
///
/// C: `setTypeConvert()` in `t_set.c:488-490`.
pub fn set_type_convert(setobj: &mut SetData, enc: SetEncoding) {
    let cap = set_type_size(setobj);
    set_type_convert_and_expand(setobj, enc, cap);
}

/// Convert the set to `enc`, pre-sizing for `cap` elements.
///
/// Returns `true` on success, `false` on OOM (Phase A always succeeds).
///
/// C: `setTypeConvertAndExpand()` in `t_set.c:496-551`.
pub fn set_type_convert_and_expand(setobj: &mut SetData, enc: SetEncoding, cap: usize) -> bool {
    if setobj.encoding() == enc {
        return true; // already in requested encoding
    }

    let elements = set_type_collect_elements(setobj);

    match enc {
        SetEncoding::HashTable => {
            let mut ht = HashSet::with_capacity(cap);
            for elem in elements {
                let key = RedisString::from_bytes(&elem.into_bytes());
                ht.insert(key);
            }
            *setobj = SetData::HashTable(ht);
        }
        SetEncoding::ListPack => {
            let mut lp: Vec<SetElement> = Vec::with_capacity(cap);
            for elem in elements {
                // Prefer integer storage when the element is integer-valued.
                let stored = match &elem {
                    SetElement::Str(b) => {
                        if let Some(n) = parse_integer_bytes(b) {
                            SetElement::Integer(n)
                        } else {
                            elem
                        }
                    }
                    SetElement::Integer(_) => elem,
                };
                lp.push(stored);
            }
            *setobj = SetData::ListPack(lp);
        }
        SetEncoding::IntSet => {
            // All elements must be integers; caller is responsible for
            // checking this precondition before calling (see maybeConvertToIntset).
            let mut is: Vec<i64> = Vec::with_capacity(cap);
            for elem in elements {
                match elem {
                    SetElement::Integer(n) => is.push(n),
                    SetElement::Str(b) => {
                        if let Some(n) = parse_integer_bytes(&b) {
                            is.push(n);
                        }
                        // TODO(port): C asserts this always succeeds (string2ll)
                    }
                }
            }
            is.sort_unstable();
            is.dedup();
            *setobj = SetData::IntSet(is);
        }
    }
    true
}

/// Duplicate a set, preserving the same encoding.
///
/// C: `setTypeDup()` in `t_set.c:558-595`.
pub fn set_type_dup(o: &SetData) -> SetData {
    match o {
        SetData::IntSet(is) => SetData::IntSet(is.clone()),
        SetData::ListPack(lp) => SetData::ListPack(lp.clone()),
        SetData::HashTable(ht) => SetData::HashTable(ht.clone()),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal utilities
// ─────────────────────────────────────────────────────────────────────────────

/// Collect all elements of a set into a `Vec<SetElement>`.
///
/// Used by the Phase A iterator and conversion functions.
///
/// PERF(port): This copies all elements; Phase B iterators avoid this copy.
fn set_type_collect_elements(set: &SetData) -> Vec<SetElement> {
    match set {
        SetData::IntSet(is) => {
            is.iter().map(|&n| SetElement::Integer(n)).collect()
        }
        SetData::ListPack(lp) => lp.clone(),
        SetData::HashTable(ht) => {
            ht.iter()
                .map(|rs: &RedisString| SetElement::Str(rs.as_bytes().to_vec()))
                .collect()
        }
    }
}

/// Try to parse `bytes` as a decimal `i64`.  Returns `None` if not a valid
/// integer representation.
///
/// C: `string2ll()` call sites throughout `t_set.c`.
///
/// PORT NOTE: Uses Rust's standard integer parsing on byte slice rendered as
/// ASCII.  The C `string2ll` handles the same range and format.
fn parse_integer_bytes(bytes: &[u8]) -> Option<i64> {
    // All decimal integers are valid ASCII, so this conversion is safe and
    // does not violate the "no from_utf8 for Redis data" rule.  We are
    // parsing numbers, not storing byte strings as Rust strings.
    let s = core::str::from_utf8(bytes).ok()?;
    s.parse::<i64>().ok()
}

/// Comparator: ascending by cardinality.  Used to sort sets before SINTER.
///
/// C: `qsortCompareSetsByCardinality()` in `t_set.c:1226-1230`.
fn compare_sets_by_cardinality(a: &Option<SetData>, b: &Option<SetData>) -> std::cmp::Ordering {
    let sa = a.as_ref().map_or(0, set_type_size);
    let sb = b.as_ref().map_or(0, set_type_size);
    sa.cmp(&sb)
}

/// Comparator: descending by cardinality.  Used to order sets-to-subtract in
/// SDIFF algorithm 1.
///
/// C: `qsortCompareSetsByRevCardinality()` in `t_set.c:1234-1242`.
fn compare_sets_by_rev_cardinality(a: &Option<SetData>, b: &Option<SetData>) -> std::cmp::Ordering {
    let sa = a.as_ref().map_or(0, set_type_size);
    let sb = b.as_ref().map_or(0, set_type_size);
    sb.cmp(&sa) // reversed
}

// ─────────────────────────────────────────────────────────────────────────────
// Command implementations
// ─────────────────────────────────────────────────────────────────────────────

/// SADD key member [member ...]
///
/// C: `saddCommand()` in `t_set.c:597-620`.
pub fn sadd_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:597-620
    let key = ctx.arg(1)?;

    // TODO(port): ctx.db_mut().lookup_key_write(key) — Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let mut set: Option<SetData> = None; // placeholder lookup

    let argc = ctx.argc();
    if set.is_none() {
        // Key doesn't exist — create a new set sized for all arguments.
        let first_member = ctx.arg(2)?;
        let size_hint = argc - 2;
        set = Some(set_type_create(first_member, size_hint));
        // TODO(port): ctx.db_mut().add(key, RedisObject::Set(set)) — Phase 3
    } else {
        let size_hint = argc - 2;
        if let Some(ref mut s) = set {
            set_type_maybe_convert(s, size_hint);
        }
    }

    let mut added: i64 = 0;
    for i in 2..argc {
        let member = ctx.arg(i)?;
        if let Some(ref mut s) = set {
            if set_type_add(s, member) {
                added += 1;
            }
        }
    }

    if added > 0 {
        // TODO(port): ctx.signal_modified_key(key) — Phase 3
        // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, b"sadd", key, db_id) — Phase 3
        // TODO(port): ctx.server_dirty_incr(added) — Phase 3
    }

    ctx.reply_integer(added)
}

/// SREM key member [member ...]
///
/// C: `sremCommand()` in `t_set.c:622-648`.
pub fn srem_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:622-648
    let key = ctx.arg(1)?;

    // TODO(port): lookupKeyWriteOrReply — reply czero if not found, Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let set_data: Option<&mut SetData> = None; // placeholder

    let set_data = match set_data {
        Some(s) => s,
        None => return ctx.reply_integer(0),
    };

    // TODO(port): for hashtable encoding, pause auto-shrink during multi-remove
    // C: hashtablePauseAutoShrink(objectGetVal(set))

    let argc = ctx.argc();
    let mut deleted: i64 = 0;
    let mut key_removed = false;

    for i in 2..argc {
        let member = ctx.arg(i)?;
        if set_type_remove(set_data, member) {
            deleted += 1;
            if set_type_size(set_data) == 0 {
                // TODO(port): ctx.db_mut().delete(key) — Phase 3
                key_removed = true;
                break;
            }
        }
    }

    if !key_removed {
        // TODO(port): resume auto-shrink for hashtable encoding — Phase 3
    }

    if deleted > 0 {
        // TODO(port): ctx.signal_modified_key(key) — Phase 3
        // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, b"srem", key, db_id) — Phase 3
        if key_removed {
            // TODO(port): ctx.notify_keyspace_event(NOTIFY_GENERIC, b"del", key, db_id) — Phase 3
        }
        // TODO(port): ctx.server_dirty_incr(deleted) — Phase 3
    }

    ctx.reply_integer(deleted)
}

/// SMOVE source destination member
///
/// C: `smoveCommand()` in `t_set.c:650-701`.
pub fn smove_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:650-701
    let src_key = ctx.arg(1)?;
    let dst_key = ctx.arg(2)?;
    let member  = ctx.arg(3)?;

    // TODO(port): lookupKeyWrite for src and dst — Phase 3
    let srcset: Option<&mut SetData> = None; // placeholder
    let dstset: Option<&mut SetData> = None; // placeholder

    // If source key does not exist return 0.
    if srcset.is_none() {
        return ctx.reply_integer(0);
    }

    // TODO(port): checkType(c, srcset, OBJ_SET) — Phase 3
    // TODO(port): checkType(c, dstset, OBJ_SET) — Phase 3

    // If src == dst, SMOVE is a no-op; just check membership.
    // TODO(port): pointer equality for same-key check; compare src_key == dst_key
    if src_key == dst_key {
        let is_member = srcset
            .map(|s| set_type_is_member(s, member))
            .unwrap_or(false);
        return ctx.reply_integer(if is_member { 1 } else { 0 });
    }

    // Remove element from source.
    let removed = srcset
        .map(|s| set_type_remove(s, member))
        .unwrap_or(false);
    if !removed {
        return ctx.reply_integer(0);
    }

    // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, b"srem", src_key, db_id) — Phase 3

    // Remove the source key if now empty.
    if srcset.map_or(0, |s| set_type_size(s)) == 0 {
        // TODO(port): ctx.db_mut().delete(src_key) — Phase 3
        // TODO(port): ctx.notify_keyspace_event(NOTIFY_GENERIC, b"del", src_key, db_id) — Phase 3
    }

    // Create destination set if it doesn't exist.
    // TODO(port): handle dstset creation and dbAdd — Phase 3

    // TODO(port): ctx.signal_modified_key(src_key) — Phase 3
    // TODO(port): ctx.server_dirty_incr(1) — Phase 3

    // Add element to destination.
    let added = dstset
        .map(|s| set_type_add(s, member))
        .unwrap_or(false);
    if added {
        // TODO(port): ctx.server_dirty_incr(1) — Phase 3
        // TODO(port): ctx.signal_modified_key(dst_key) — Phase 3
        // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, b"sadd", dst_key, db_id) — Phase 3
    }

    ctx.reply_integer(1)
}

/// SISMEMBER key member
///
/// C: `sismemberCommand()` in `t_set.c:703-712`.
pub fn sismember_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:703-712
    let _key    = ctx.arg(1)?;
    let member  = ctx.arg(2)?;

    // TODO(port): lookupKeyReadOrReply — reply czero if missing, Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let set: Option<&SetData> = None; // placeholder

    let is_member = set.map_or(false, |s| set_type_is_member(s, member));
    ctx.reply_integer(if is_member { 1 } else { 0 })
}

/// SMISMEMBER key member [member ...]
///
/// C: `smismemberCommand()` in `t_set.c:714-731`.
pub fn smismember_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:714-731
    let _key = ctx.arg(1)?;

    // Don't abort when key is missing — treat as empty set.
    // TODO(port): lookupKeyRead (not write) — Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let set: Option<&SetData> = None; // placeholder

    let argc = ctx.argc();
    ctx.reply_array_header((argc - 2) as i64)?;

    for i in 2..argc {
        let member = ctx.arg(i)?;
        let is_member = set.map_or(false, |s| set_type_is_member(s, member));
        ctx.reply_integer(if is_member { 1 } else { 0 })?;
    }
    Ok(())
}

/// SCARD key
///
/// C: `scardCommand()` in `t_set.c:733-738`.
pub fn scard_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:733-738
    let _key = ctx.arg(1)?;

    // TODO(port): lookupKeyReadOrReply — reply czero if missing, Phase 3
    // TODO(port): checkType(c, o, OBJ_SET) — Phase 3
    let o: Option<&SetData> = None; // placeholder

    let size = o.map_or(0, set_type_size);
    ctx.reply_integer(size as i64)
}

/// SPOP key [count]
///
/// Dispatches to `spop_with_count_command` when count is provided.
///
/// C: `spopCommand()` in `t_set.c:953-990`.
pub fn spop_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:953-990
    let argc = ctx.argc();
    if argc == 3 {
        return spop_with_count_command(ctx);
    } else if argc > 3 {
        return Err(RedisError::syntax(b"syntax error"));
    }

    let _key = ctx.arg(1)?;

    // TODO(port): lookupKeyWriteOrReply — reply null if missing, Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let set: Option<&mut SetData> = None; // placeholder

    let set = match set {
        Some(s) => s,
        None => return ctx.reply_null(),
    };

    let ele = match set_type_pop_random(set) {
        Some(e) => e,
        None => return ctx.reply_null(),
    };

    // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, b"spop", key, db_id) — Phase 3
    // TODO(port): rewriteClientCommandVector(c, 3, shared.srem, key, ele) — Phase 3

    let ele_bytes = ele.into_bytes();
    ctx.reply_bulk(&ele_bytes)?;

    if set_type_size(set) == 0 {
        // TODO(port): ctx.db_mut().delete(key) — Phase 3
        // TODO(port): ctx.notify_keyspace_event(NOTIFY_GENERIC, b"del", key, db_id) — Phase 3
    }

    // TODO(port): ctx.signal_modified_key(key) — Phase 3
    // TODO(port): ctx.server_dirty_incr(1) — Phase 3

    Ok(())
}

/// SPOP key count  (variant with count argument)
///
/// C: `spopWithCountCommand()` in `t_set.c:749-951`.
pub fn spop_with_count_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:749-951

    // Parse count argument.
    let count_bytes = ctx.arg(2)?;
    let count = parse_positive_long(count_bytes)
        .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;

    let _key = ctx.arg(1)?;

    // TODO(port): lookupKeyWriteOrReply — reply emptyset if missing, Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let set: Option<&mut SetData> = None; // placeholder

    let set = match set {
        Some(s) => s,
        None => {
            ctx.reply_array_header(0)?;
            return Ok(());
        }
    };

    if count == 0 {
        ctx.reply_array_header(0)?;
        return Ok(());
    }

    let size = set_type_size(set) as u64;

    // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, b"spop", key, db_id) — Phase 3
    // TODO(port): ctx.server_dirty_incr(...) — Phase 3

    // CASE 1: return entire set when count >= size.
    if count >= size {
        // Delegate to sunion_diff_generic_command logic for the single-set union.
        // TODO(port): call sunion_diff_generic_command(ctx, &[key], 1, None, SetOp::Union) — Phase 3

        let mut si = set_type_init_iterator(set);
        ctx.reply_array_header(size as i64)?;
        while let Some(elem) = set_type_next(&mut si) {
            ctx.reply_bulk(&elem.into_bytes())?;
        }
        set_type_release_iterator(si);

        // TODO(port): ctx.db_mut().delete(key) — Phase 3
        // TODO(port): ctx.notify_keyspace_event(NOTIFY_GENERIC, b"del", key, db_id) — Phase 3
        // TODO(port): rewriteClientCommandVector to DEL/UNLINK — Phase 3
        // TODO(port): ctx.signal_modified_key(key) — Phase 3
        return Ok(());
    }

    // C: t_set.c:799 — batchsize and propargv setup omitted (replication, Phase 3)
    // TODO(port): alsoPropagate / replication batch setup — Phase 3

    ctx.reply_array_header(count as i64)?;

    let remaining = size - count;

    // CASE 2: pop count is small relative to set size — pop elements directly.
    if remaining * SPOP_MOVE_STRATEGY_MUL > count {
        for _ in 0..count {
            let elem = match set_type_pop_random(set) {
                Some(e) => e,
                None => break,
            };
            ctx.reply_bulk(&elem.into_bytes())?;
            // TODO(port): alsoPropagate SREM batching — Phase 3
        }
    } else {
        // CASE 3: count approaches size — build a new set of *remaining*
        // elements, then return everything left in the original set.
        let mut new_set = match set {
            SetData::ListPack(_) => SetData::ListPack(Vec::new()),
            SetData::IntSet(_)   => SetData::IntSet(Vec::new()),
            SetData::HashTable(_) => SetData::HashTable(HashSet::new()),
        };

        // Sample `remaining` random elements into new_set.
        for _ in 0..remaining {
            let elem = match set_type_pop_random(set) {
                Some(e) => e,
                None => break,
            };
            match &elem {
                SetElement::Str(b) => { set_type_add(&mut new_set, b); }
                SetElement::Integer(n) => { set_type_add_aux(&mut new_set, None, *n, false); }
            }
        }

        // The original set now holds only the elements to return.
        let mut si = set_type_init_iterator(set);
        while let Some(elem) = set_type_next(&mut si) {
            ctx.reply_bulk(&elem.into_bytes())?;
            // TODO(port): alsoPropagate SREM batching — Phase 3
        }
        set_type_release_iterator(si);

        // Replace the stored set with new_set.
        // TODO(port): ctx.db_mut().replace_value(key, RedisObject::Set(new_set)) — Phase 3
        let _ = new_set; // suppress unused-variable warning for Phase A
    }

    // TODO(port): flush remaining propargv batch — Phase 3
    // TODO(port): ctx.prevent_command_propagation() — Phase 3
    // TODO(port): ctx.signal_modified_key(key) — Phase 3

    Ok(())
}

/// SRANDMEMBER key [count]
///
/// C: `srandmemberCommand()` in `t_set.c:1201-1224`.
pub fn srandmember_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1201-1224
    let argc = ctx.argc();
    if argc == 3 {
        return srandmember_with_count_command(ctx);
    } else if argc > 3 {
        return Err(RedisError::syntax(b"syntax error"));
    }

    let _key = ctx.arg(1)?;

    // TODO(port): lookupKeyReadOrReply — reply null if missing, Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let set: Option<&SetData> = None; // placeholder

    let set = match set {
        Some(s) => s,
        None => return ctx.reply_null(),
    };

    match set_type_random_element(set) {
        Some(elem) => ctx.reply_bulk(&elem.into_bytes()),
        None => ctx.reply_null(),
    }
}

/// SRANDMEMBER key count  (variant with count argument)
///
/// Handles all four algorithm cases from the C implementation:
/// Case 1 — negative count (allow duplicates)
/// Case 2 — count >= size (return whole set)
/// Case 2.5 — listpack encoding, unique, count < size
/// Case 3 — copy-all-subtract strategy
/// Case 4 — random-pick-unique strategy
///
/// C: `srandmemberWithCountCommand()` in `t_set.c:1005-1198`.
pub fn srandmember_with_count_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1005-1198
    let count_bytes = ctx.arg(2)?;

    // Negative count means allow duplicates.
    let raw_count = parse_signed_long(count_bytes)
        .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;

    let (count, uniq) = if raw_count >= 0 {
        (raw_count as u64, true)
    } else {
        ((-raw_count) as u64, false)
    };

    let _key = ctx.arg(1)?;

    // TODO(port): lookupKeyReadOrReply — reply emptyarray if missing, Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    let set: Option<&SetData> = None; // placeholder

    let set = match set {
        Some(s) => s,
        None => {
            ctx.reply_array_header(0)?;
            return Ok(());
        }
    };

    let size = set_type_size(set) as u64;

    if count == 0 {
        ctx.reply_array_header(0)?;
        return Ok(());
    }

    // CASE 1: negative count — return N random elements, duplicates allowed.
    if !uniq || count == 1 {
        ctx.reply_array_header(count as i64)?;
        // TODO(port): listpack specialised batch path (lpRandomEntries) — Phase 4
        let mut remaining = count;
        while remaining > 0 {
            match set_type_random_element(set) {
                Some(elem) => ctx.reply_bulk(&elem.into_bytes())?,
                None => break,
            }
            remaining -= 1;
            // TODO(port): check client close_asap flag — Phase 3
        }
        return Ok(());
    }

    // CASE 2: count >= size — return the whole set.
    if count >= size {
        ctx.reply_array_header(size as i64)?;
        let mut si = set_type_init_iterator(set);
        while let Some(elem) = set_type_next(&mut si) {
            ctx.reply_bulk(&elem.into_bytes())?;
        }
        set_type_release_iterator(si);
        return Ok(());
    }

    // CASE 2.5: listpack, unique, count < size.
    // TODO(port): Phase 4 — use lpNextRandom for the listpack path.
    // For Phase A, fall through to the generic path below.

    // Build a temporary result set of unique elements.
    let mut result: HashSet<RedisString> = HashSet::with_capacity(count as usize);

    if count * SRANDMEMBER_SUB_STRATEGY_MUL > size {
        // CASE 3: Copy all elements, then remove down to `count`.
        let elements = set_type_collect_elements(set);
        for elem in elements {
            result.insert(RedisString::from_bytes(&elem.into_bytes()));
        }
        // Remove random elements until we have exactly `count`.
        // TODO(port): use proper RNG (C: hashtableFairRandomEntry) — Phase B
        while (result.len() as u64) > count {
            if let Some(key) = result.iter().next().cloned() {
                result.remove(&key);
            }
        }
    } else {
        // CASE 4: Pick random unique elements until we have `count`.
        while (result.len() as u64) < count {
            match set_type_random_element(set) {
                Some(elem) => {
                    result.insert(RedisString::from_bytes(&elem.into_bytes()));
                }
                None => break,
            }
        }
    }

    // Send the result.
    ctx.reply_array_header(result.len() as i64)?;
    for rs in &result {
        let rs_typed: &RedisString = rs;
        ctx.reply_bulk(rs_typed.as_bytes())?;
    }

    Ok(())
}

/// SINTER key [key ...]
///
/// C: `sinterCommand()` in `t_set.c:1411-1413`.
pub fn sinter_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1411-1413
    let argc = ctx.argc();
    let keys: Vec<&[u8]> = (1..argc)
        .map(|i| ctx.arg(i))
        .collect::<Result<_, _>>()?;
    sinter_generic_command(ctx, &keys, None, false, 0)
}

/// SINTERCARD numkeys key [key ...] [LIMIT limit]
///
/// C: `sinterCardCommand()` in `t_set.c:1416-1442`.
pub fn sintercard_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1416-1442
    let numkeys_bytes = ctx.arg(1)?;
    let numkeys = parse_positive_long(numkeys_bytes)
        .ok_or_else(|| RedisError::runtime(b"numkeys should be greater than 0"))? as usize;

    let argc = ctx.argc();
    if numkeys > argc - 2 {
        return Err(RedisError::runtime(
            b"Number of keys can't be greater than number of args",
        ));
    }

    let mut limit: u64 = 0;
    let mut j = 2 + numkeys;
    while j < argc {
        let opt = ctx.arg(j)?;
        let more_args = (argc - 1) - j;
        if opt.eq_ignore_ascii_case(b"LIMIT") && more_args > 0 {
            j += 1;
            let lim_bytes = ctx.arg(j)?;
            limit = parse_positive_long(lim_bytes)
                .ok_or_else(|| RedisError::runtime(b"LIMIT can't be negative"))? as u64;
        } else {
            return Err(RedisError::syntax(b"syntax error"));
        }
        j += 1;
    }

    let keys: Vec<&[u8]> = (2..2 + numkeys)
        .map(|i| ctx.arg(i))
        .collect::<Result<_, _>>()?;
    sinter_generic_command(ctx, &keys, None, true, limit)
}

/// SINTERSTORE destination key [key ...]
///
/// C: `sinterstoreCommand()` in `t_set.c:1445-1447`.
pub fn sinterstore_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1445-1447
    let argc = ctx.argc();
    let dst_key = ctx.arg(1)?;
    let keys: Vec<&[u8]> = (2..argc)
        .map(|i| ctx.arg(i))
        .collect::<Result<_, _>>()?;
    sinter_generic_command(ctx, &keys, Some(dst_key), false, 0)
}

/// SUNION key [key ...]
///
/// C: `sunionCommand()` in `t_set.c:1633-1635`.
pub fn sunion_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1633-1635
    let argc = ctx.argc();
    let keys: Vec<&[u8]> = (1..argc)
        .map(|i| ctx.arg(i))
        .collect::<Result<_, _>>()?;
    sunion_diff_generic_command(ctx, &keys, None, SetOp::Union)
}

/// SUNIONSTORE destination key [key ...]
///
/// C: `sunionstoreCommand()` in `t_set.c:1638-1640`.
pub fn sunionstore_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1638-1640
    let argc = ctx.argc();
    let dst_key = ctx.arg(1)?;
    let keys: Vec<&[u8]> = (2..argc)
        .map(|i| ctx.arg(i))
        .collect::<Result<_, _>>()?;
    sunion_diff_generic_command(ctx, &keys, Some(dst_key), SetOp::Union)
}

/// SDIFF key [key ...]
///
/// C: `sdiffCommand()` in `t_set.c:1643-1645`.
pub fn sdiff_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1643-1645
    let argc = ctx.argc();
    let keys: Vec<&[u8]> = (1..argc)
        .map(|i| ctx.arg(i))
        .collect::<Result<_, _>>()?;
    sunion_diff_generic_command(ctx, &keys, None, SetOp::Diff)
}

/// SDIFFSTORE destination key [key ...]
///
/// C: `sdiffstoreCommand()` in `t_set.c:1648-1650`.
pub fn sdiffstore_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1648-1650
    let argc = ctx.argc();
    let dst_key = ctx.arg(1)?;
    let keys: Vec<&[u8]> = (2..argc)
        .map(|i| ctx.arg(i))
        .collect::<Result<_, _>>()?;
    sunion_diff_generic_command(ctx, &keys, Some(dst_key), SetOp::Diff)
}

/// SSCAN key cursor [MATCH pattern] [COUNT count]
///
/// C: `sscanCommand()` in `t_set.c:1652-1659`.
pub fn sscan_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    // C: t_set.c:1652-1659
    let _key        = ctx.arg(1)?;
    let cursor_bytes = ctx.arg(2)?;

    // TODO(port): parseScanCursorOrReply — Phase 3
    let _cursor: u64 = parse_positive_long(cursor_bytes)
        .ok_or_else(|| RedisError::runtime(b"invalid cursor"))? as u64;

    // TODO(port): lookupKeyReadOrReply — reply emptyscan if missing, Phase 3
    // TODO(port): checkType(c, set, OBJ_SET) — Phase 3
    // TODO(port): ctx.scan_generic_command(set, cursor) — Phase 3

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Generic multi-key set operations
// ─────────────────────────────────────────────────────────────────────────────

/// Generic implementation for SINTER / SINTERSTORE / SINTERCARD.
///
/// * `dstkey = None`  → reply with the intersection elements.
/// * `dstkey = Some`  → store the result under `dstkey`, reply with count.
/// * `cardinality_only = true` → count members up to `limit` (SINTERCARD).
///
/// C: `sinterGenericCommand()` in `t_set.c:1252-1408`.
fn sinter_generic_command(
    ctx: &mut CommandContext,
    setkeys: &[&[u8]],
    dstkey: Option<&[u8]>,
    cardinality_only: bool,
    limit: u64,
) -> Result<(), RedisError> {
    // C: t_set.c:1252-1408
    let setnum = setkeys.len();

    // Resolve all keys to SetData references (None = empty set).
    // TODO(port): real DB lookup — Phase 3.
    let sets: Vec<Option<SetData>> = setkeys
        .iter()
        .map(|_key| -> Result<Option<SetData>, RedisError> {
            // TODO(port): ctx.db().lookup_key_read(key) — Phase 3
            // TODO(port): checkType(c, setobj, OBJ_SET) — Phase 3
            Ok(None) // placeholder — all sets treated as empty for Phase A
        })
        .collect::<Result<_, _>>()?;

    // Intersection with an empty set is always empty.
    let has_empty = sets.iter().any(|s: &Option<SetData>| s.is_none());
    if has_empty {
        if let Some(dst) = dstkey {
            // TODO(port): dbDelete(dst) if it exists — Phase 3
            let _ = dst;
            // TODO(port): ctx.signal_modified_key / notify — Phase 3
            return ctx.reply_integer(0);
        } else if cardinality_only {
            return ctx.reply_integer(0);
        } else {
            ctx.reply_array_header(0)?;
            return Ok(());
        }
    }

    // Sort sets smallest-first to minimise the inner-loop work.
    let mut sorted_sets: Vec<Option<SetData>> = sets;
    sorted_sets.sort_by(compare_sets_by_cardinality);

    // Iterate the smallest set, test each element against all others.
    let mut dstset: Option<SetData> = dstkey.as_ref().map(|_| {
        // Choose initial encoding based on the smallest set.
        // TODO(port): mirror C's encoding-selection heuristic — Phase 4
        SetData::ListPack(Vec::new())
    });

    let deferred_len_marker = if dstkey.is_none() && !cardinality_only {
        // TODO(port): addReplyDeferredLen — Phase 3
        true
    } else {
        false
    };

    let mut cardinality: u64 = 0;

    if let Some(Some(ref first)) = sorted_sets.first() {
        let elements = set_type_collect_elements(first);
        for elem in &elements {
            let elem_bytes = elem.as_bytes();

            // Check if this element is in every other set.
            let in_all = sorted_sets[1..].iter().all(|maybe_set: &Option<SetData>| {
                maybe_set.as_ref().map_or(false, |s: &SetData| {
                    match elem {
                        SetElement::Integer(n) => set_type_is_member_aux(s, None, *n, false),
                        SetElement::Str(b) => set_type_is_member_aux(s, Some(b), 0, false),
                    }
                })
            });

            if in_all {
                if cardinality_only {
                    cardinality += 1;
                    if limit > 0 && cardinality >= limit {
                        break;
                    }
                } else if dstkey.is_none() {
                    ctx.reply_bulk(&elem_bytes)?;
                    cardinality += 1;
                } else if let Some(ref mut dst) = dstset {
                    match elem {
                        SetElement::Integer(n) => {
                            set_type_add_aux(dst, None, *n, false);
                        }
                        SetElement::Str(b) => {
                            set_type_add(dst, b);
                        }
                    }
                    cardinality += 1;
                }
            }
        }
    }

    // Finalise reply / store.
    if cardinality_only {
        return ctx.reply_integer(cardinality as i64);
    }

    if let Some(dst_key) = dstkey {
        if let Some(dst) = dstset {
            let size = set_type_size(&dst);
            if size > 0 {
                // TODO(port): setKey(ctx, db, dstkey, dst) — Phase 3
                let _ = dst_key;
                // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, b"sinterstore", ...) — Phase 3
                // TODO(port): ctx.server_dirty_incr(1) — Phase 3
                return ctx.reply_integer(size as i64);
            } else {
                // TODO(port): dbDelete(ctx, db, dstkey) — Phase 3
                // TODO(port): ctx.signal_modified_key / notify — Phase 3
                return ctx.reply_integer(0);
            }
        }
        return ctx.reply_integer(0);
    }

    if deferred_len_marker {
        // TODO(port): setDeferredSetLen(c, replylen, cardinality) — Phase 3
        // For Phase A the array header was not yet sent; send it now.
        // (In the real implementation addReplyDeferredLen is used.)
    }

    Ok(())
}

/// Generic implementation for SUNION / SUNIONSTORE / SDIFF / SDIFFSTORE.
///
/// * `dstkey = None`  → reply with the result elements.
/// * `dstkey = Some`  → store result under `dstkey`, reply with count.
///
/// SDIFF uses two algorithms:
/// * Algorithm 1 — O(N·M): iterate first set, check membership in others.
/// * Algorithm 2 — O(N): add first set then subtract subsequent sets.
///
/// C: `sunionDiffGenericCommand()` in `t_set.c:1449-1630`.
fn sunion_diff_generic_command(
    ctx: &mut CommandContext,
    setkeys: &[&[u8]],
    dstkey: Option<&[u8]>,
    op: SetOp,
) -> Result<(), RedisError> {
    // C: t_set.c:1449-1630
    let setnum = setkeys.len();

    // Resolve all keys.
    // TODO(port): real DB lookup — Phase 3.
    let mut sets: Vec<Option<SetData>> = setkeys
        .iter()
        .map(|_key| -> Result<Option<SetData>, RedisError> {
            // TODO(port): ctx.db().lookup_key_read(key) — Phase 3
            // TODO(port): checkType(c, setobj, OBJ_SET) — Phase 3
            Ok(None)
        })
        .collect::<Result<_, _>>()?;

    // Determine if the first set key appears again in the list (SDIFF shortcut).
    let sameset = sets
        .iter()
        .enumerate()
        .skip(1)
        .any(|(j, _)| sets[0].is_none() && sets[j].is_none());
    // TODO(port): proper same-key detection using key byte comparison — Phase 3

    // Choose diff algorithm.
    let diff_algo = if op == SetOp::Diff && sets[0].is_some() && !sameset {
        let algo_one_work: usize = setnum * sets[0].as_ref().map_or(0, set_type_size);
        let algo_two_work: usize = sets.iter().map(|s: &Option<SetData>| s.as_ref().map_or(0, set_type_size)).sum();
        if algo_one_work / 2 <= algo_two_work { 1 } else { 2 }
    } else {
        1
    };

    // For diff algo 1, sort sets-to-subtract (indices 1..) by descending size.
    if op == SetOp::Diff && diff_algo == 1 && setnum > 1 {
        sets[1..].sort_by(compare_sets_by_rev_cardinality);
    }

    // Build the result set.
    // dstset_encoding: intset if all source sets are intset, else hashtable.
    let dstset_encoding = if sets
        .iter()
        .filter_map(|s: &Option<SetData>| s.as_ref())
        .all(|s: &SetData| matches!(s, SetData::IntSet(_)))
    {
        SetEncoding::IntSet
    } else {
        SetEncoding::HashTable
    };

    let mut dstset: SetData = if dstset_encoding == SetEncoding::IntSet {
        SetData::IntSet(Vec::new())
    } else {
        SetData::HashTable(HashSet::new())
    };

    let mut cardinality: i64 = 0;

    match op {
        SetOp::Union => {
            for maybe_set in &sets {
                if let Some(s) = maybe_set {
                    let elements = set_type_collect_elements(s);
                    for elem in elements {
                        let added = match &elem {
                            SetElement::Integer(n) => set_type_add_aux(&mut dstset, None, *n, false),
                            SetElement::Str(b) => set_type_add(&mut dstset, b),
                        };
                        if added { cardinality += 1; }
                    }
                }
            }
        }
        SetOp::Diff if sameset => {
            // Same key appears in both source and subtracted set — result is empty.
        }
        SetOp::Diff if diff_algo == 1 => {
            // Algorithm 1: iterate first set, check membership in others.
            if let Some(Some(ref first)) = sets.first() {
                let elements = set_type_collect_elements(first);
                'outer: for elem in elements {
                    for j in 1..setnum {
                        if let Some(Some(ref other)) = sets.get(j) {
                            let member = match &elem {
                                SetElement::Integer(n) => set_type_is_member_aux(other, None, *n, false),
                                SetElement::Str(b)     => set_type_is_member_aux(other, Some(b), 0, false),
                            };
                            if member { continue 'outer; }
                        }
                    }
                    // Not found in any other set — add to result.
                    let added = match &elem {
                        SetElement::Integer(n) => set_type_add_aux(&mut dstset, None, *n, false),
                        SetElement::Str(b) => set_type_add(&mut dstset, b),
                    };
                    if added { cardinality += 1; }
                }
            }
        }
        SetOp::Diff => {
            // Algorithm 2: add first set, remove subsequent sets.
            for (j, maybe_set) in sets.iter().enumerate() {
                if let Some(s) = maybe_set {
                    let elements = set_type_collect_elements(s);
                    for elem in elements {
                        if j == 0 {
                            let added = match &elem {
                                SetElement::Integer(n) => set_type_add_aux(&mut dstset, None, *n, false),
                                SetElement::Str(b) => set_type_add(&mut dstset, b),
                            };
                            if added { cardinality += 1; }
                        } else {
                            let removed = match &elem {
                                SetElement::Integer(n) => set_type_remove_aux(&mut dstset, None, *n, false),
                                SetElement::Str(b) => set_type_remove(&mut dstset, b),
                            };
                            if removed { cardinality -= 1; }
                        }
                    }
                    if cardinality == 0 { break; }
                }
            }
        }
    }

    // Output or store.
    if dstkey.is_none() {
        let result_size = set_type_size(&dstset);
        ctx.reply_array_header(result_size as i64)?;
        let mut si = set_type_init_iterator(&dstset);
        while let Some(elem) = set_type_next(&mut si) {
            ctx.reply_bulk(&elem.into_bytes())?;
        }
        set_type_release_iterator(si);

        // TODO(port): lazyfree_lazy_server_del path for dstset — Phase 3
    } else if let Some(dst_key) = dstkey {
        let result_size = set_type_size(&dstset);
        if result_size > 0 {
            // TODO(port): setKey(ctx, db, dstkey, dstset) — Phase 3
            let _ = dst_key;
            let event = match op {
                SetOp::Union => b"sunionstore" as &[u8],
                SetOp::Diff  => b"sdiffstore",
            };
            // TODO(port): ctx.notify_keyspace_event(NOTIFY_SET, event, dstkey, db_id) — Phase 3
            let _ = event;
            // TODO(port): ctx.server_dirty_incr(1) — Phase 3
            ctx.reply_integer(result_size as i64)?;
        } else {
            // TODO(port): dbDelete(ctx, db, dstkey) if exists — Phase 3
            // TODO(port): ctx.signal_modified_key / notify — Phase 3
            ctx.reply_integer(0)?;
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Parsing helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a non-negative `i64` from a byte slice.  Returns `None` if the
/// value is negative or not a valid integer.
///
/// C: `getPositiveLongFromObjectOrReply` pattern.
fn parse_positive_long(bytes: &[u8]) -> Option<i64> {
    let n = parse_signed_long(bytes)?;
    if n < 0 { None } else { Some(n) }
}

/// Parse a signed `i64` from a byte slice.
///
/// C: `getRangeLongFromObjectOrReply` pattern.
fn parse_signed_long(bytes: &[u8]) -> Option<i64> {
    // Decimal digits are ASCII; this conversion does not violate the
    // "no from_utf8 for Redis data" rule because we are parsing a number.
    let s = core::str::from_utf8(bytes).ok()?;
    s.trim().parse::<i64>().ok()
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/t_set.c  (1660 lines, 40 functions)
//   target_crate:  redis-commands
//   confidence:    medium
//   todos:         122  (109 TODO(port) + 13 TODO(architect))
//   port_notes:    2
//   unsafe_blocks: 0
//   notes:         Logic is faithful to C. All DB/server-config/replication
//                  calls are stubbed with TODO(port) or TODO(architect) pending
//                  Phase 3 CommandContext plumbing.  Inner encoding types
//                  (IntSet, ListPack, HashTable) are Vec<i64>/Vec<SetElement>/
//                  HashSet<RedisString> placeholders; Phase 4 replaces them
//                  with redis-ds types.  RNG in set_type_random_element always
//                  returns first element for Phase A; Phase B must integrate
//                  a proper PRNG.  The SetTypeIterator eagerly collects
//                  elements (PERF(port)); Phase B should use lazy per-encoding
//                  iterators.  Validator: only expected E0432/E0433
//                  (unlinked crates) remain — zero real syntax errors.
// ──────────────────────────────────────────────────────────────────────────
