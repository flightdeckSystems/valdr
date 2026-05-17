//! Bit operations: SETBIT, GETBIT, BITOP, BITCOUNT, BITPOS, BITFIELD, BITFIELD_RO.
//!
//! Port of `src/bitops.c` (1432 lines, ~25 functions).  Low-level primitives
//! (`server_popcount`, `server_bitpos`, bitfield get/set, overflow checkers)
//! are `pub(crate)` for reuse in tests and other modules.
//!
//! All Redis data (keys, values, arguments) uses `&[u8]` / `Vec<u8>` /
//! `RedisString`.  `String` / `&str` / `from_utf8` are banned per PORTING.md §1.
//!
//! ## Design notes
//!
//! - SIMD popcount (AVX2, NEON): replaced by `u8::count_ones()` which the
//!   compiler will auto-vectorise; marked PERF(port) at the call site.
//! - Word-aligned fast paths in BITOP and `server_bitpos` require unsafe; replaced
//!   with byte-by-byte loops.  PERF(port) markers indicate where to profile.
//! - C `goto` labels (`goto result:`, `goto handle_wrap:`) are restructured as
//!   Rust labeled blocks (`'label: { break 'label val; }`).
//! - Mutable-borrow/reply interleaving (C: modify robj ptr then addReply on same
//!   client*) is noted with TODO(port); Phase B will resolve via `CommandContext`
//!   split-borrow or a staging-buffer pattern.
//!
//! ## Architect items
//!
//! TODO(architect): `CommandContext::must_obey_client() -> bool` — master-client
//! flag used in `get_bit_offset_from_arg` to bypass the 512 MB string limit.
//!
//! TODO(architect): `CommandContext::proto_max_bulk_len() -> usize` — server config
//! accessor; currently hard-coded to Valkey default 512 MiB (1 << 29).
//!
//! TODO(architect): `CommandContext::signal_modified_key(key)` — WATCH / tracking.
//!
//! TODO(architect): `CommandContext::notify_keyspace_event(event_type, name, key)`
//! — keyspace event dispatch.
//!
//! TODO(architect): `CommandContext::mark_dirty(n)` — increment `server.dirty`.
//!
//! TODO(architect): `CommandContext::db_mut()` returning `&mut RedisDb` — required
//! for write operations (`db_add`, `db_delete`, `set_key`).
//!
//! TODO(architect): `CommandContext::begin_deferred_array()` /
//! `commit_deferred_array()` — deferred array-length RESP protocol used by BITFIELD.
//!
//! TODO(architect): `RedisDb::lookup_string_for_bit_write(key, min_bytes)` — should
//! encapsulate the grow-zero logic from `lookup_string_for_bit_command`.

use redis_core::command_context::CommandContext;
use redis_core::object::RedisObject;
use redis_types::{RedisError, RedisString};

// ── Overflow type constants (C: BFOVERFLOW_WRAP / SAT / FAIL) ─────────────────

/// Overflow handling mode for BITFIELD operations.
///
/// C: `BFOVERFLOW_WRAP = 0`, `BFOVERFLOW_SAT = 1`, `BFOVERFLOW_FAIL = 2`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverflowType {
    Wrap,
    Sat,
    Fail,
}

impl Default for OverflowType {
    fn default() -> Self {
        OverflowType::Wrap
    }
}

// ── Bitwise operation variants (C: BITOP_AND / OR / XOR / NOT) ────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitOp {
    And,
    Or,
    Xor,
    Not,
}

// ── BITFIELD subcommand opcodes (C: BITFIELDOP_GET / SET / INCRBY) ────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitfieldOpCode {
    Get,
    Set,
    IncrBy,
}

// ── A single parsed BITFIELD sub-operation ────────────────────────────────────

/// Parsed BITFIELD sub-operation.
///
/// C: `struct bitfieldOp` in `bitops.c`.
struct BitfieldOp {
    /// Bit offset within the bitmap.
    offset: u64,
    /// Signed SET value or INCRBY increment.
    i64: i64,
    opcode: BitfieldOpCode,
    owtype: OverflowType,
    /// Integer bitfield width in bits.
    bits: u32,
    /// `true` = signed type (`i`), `false` = unsigned type (`u`).
    sign: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// §1  Low-level bit helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Count set bits in `data`.
///
/// C: `serverPopcount(void *s, long count)` — dispatches to AVX2 / NEON / scalar.
/// PERF(port): the C version uses SIMD (AVX2 or ARM NEON) for large inputs; here we
/// rely on the compiler to auto-vectorise `count_ones()`.  Profile in Phase B with
/// large bitmaps before introducing any `unsafe` SIMD.
pub(crate) fn server_popcount(data: &[u8]) -> i64 {
    data.iter().map(|b| b.count_ones() as i64).sum()
}

/// Return the position of the first bit equal to `bit` (0 or 1) within `data[..count]`.
///
/// Semantics mirror the C function:
/// - If `bit == 0` and no clear bit is found, returns `count * 8` (zero-padded right).
/// - If `bit == 1` and no set bit is found, returns `-1`.
///
/// C: `serverBitpos(void *s, unsigned long count, int bit)` — uses word-aligned reads
/// for speed.
/// PERF(port): C version skips full words (8 bytes) at a time; this implementation
/// iterates byte-by-byte.  Profile in Phase B before adding unsafe word reads.
pub(crate) fn server_bitpos(data: &[u8], count: usize, bit: i32) -> i64 {
    let target = bit != 0;
    let skip_byte: u8 = if target { 0x00 } else { 0xFF };
    let count = count.min(data.len());
    let mut pos: i64 = 0;
    let mut i = 0usize;

    // Skip bytes that contain no target bits.
    while i < count && data[i] == skip_byte {
        pos += 8;
        i += 1;
    }

    if i >= count {
        // All bytes were "skip" bytes.
        return if target { -1 } else { pos };
    }

    // Scan the first non-skip byte bit by bit (MSB-first, matching Redis convention).
    let byte = data[i];
    for shift in (0u32..8).rev() {
        let is_set = (byte >> shift) & 1 != 0;
        if is_set == target {
            return pos;
        }
        pos += 1;
    }

    // Unreachable: a non-skip byte must contain at least one target bit.
    // TODO(architect): is panic correct here? Mirrors C's serverPanic.
    panic!("End of server_bitpos() reached.");
}

// ─────────────────────────────────────────────────────────────────────────────
// §2  Bitfield get / set primitives
// ─────────────────────────────────────────────────────────────────────────────

/// Write an unsigned integer of `bits` width into the bitmap `p` at bit `offset`.
///
/// C: `setUnsignedBitfield(unsigned char *p, uint64_t offset, uint64_t bits, uint64_t value)`
/// The bitmap treats bit 0 as the MSB of byte 0 (big-endian bit order).
pub(crate) fn set_unsigned_bitfield(p: &mut [u8], offset: u64, bits: u64, value: u64) {
    // C: bitops.c:373-386
    for j in 0..bits {
        let bitval = ((value >> (bits - 1 - j)) & 1) as u8;
        let byte_idx = ((offset + j) >> 3) as usize;
        let bit_pos = 7 - ((offset + j) & 0x7);
        let byte = p[byte_idx];
        p[byte_idx] = (byte & !(1 << bit_pos)) | (bitval << bit_pos);
    }
}

/// Write a signed integer of `bits` width into the bitmap `p` at bit `offset`.
///
/// C: `setSignedBitfield` — reinterprets `value` as unsigned then delegates.
pub(crate) fn set_signed_bitfield(p: &mut [u8], offset: u64, bits: u64, value: i64) {
    // Two's complement: cast to u64 then write as unsigned.
    set_unsigned_bitfield(p, offset, bits, value as u64);
}

/// Read an unsigned integer of `bits` width from bitmap `p` at bit `offset`.
///
/// C: `getUnsignedBitfield(unsigned char *p, uint64_t offset, uint64_t bits)`
pub(crate) fn get_unsigned_bitfield(p: &[u8], offset: u64, bits: u64) -> u64 {
    // C: bitops.c:393-405
    let mut value: u64 = 0;
    for j in 0..bits {
        let byte_idx = ((offset + j) >> 3) as usize;
        let bit_pos = 7 - ((offset + j) & 0x7);
        let byteval = p[byte_idx] as u64;
        let bitval = (byteval >> bit_pos) & 1;
        value = (value << 1) | bitval;
    }
    value
}

/// Read a signed integer of `bits` width from bitmap `p` at bit `offset`.
///
/// C: `getSignedBitfield` — reads unsigned then sign-extends if the MSB is set.
/// Relies on two's complement (guaranteed by Rust/C99 for fixed-width integers).
pub(crate) fn get_signed_bitfield(p: &[u8], offset: u64, bits: u64) -> i64 {
    // C: bitops.c:407-429
    let u = get_unsigned_bitfield(p, offset, bits);
    let mut value = u as i64;
    // Sign-extend if the MSB of the `bits`-wide field is set.
    if bits < 64 && (u & (1u64 << (bits - 1))) != 0 {
        value |= (u64::MAX << bits) as i64;
    }
    value
}

// ─────────────────────────────────────────────────────────────────────────────
// §3  Bitfield overflow checkers
// ─────────────────────────────────────────────────────────────────────────────

/// Check unsigned bitfield overflow.
///
/// Returns `(direction, limit)` where `direction` is `0` (none), `1` (overflow),
/// or `-1` (underflow); `limit` is the wrapped/saturated value (meaningless when
/// `direction == 0` or `owtype == OverflowType::Fail`).
///
/// C: `checkUnsignedBitfieldOverflow` — uses `goto handle_wrap:`.
/// Protocol guarantees unsigned bitfields ≤ 63 bits, so `value < 2^63` and casts
/// between `u64` and `i64` are safe for the arithmetic here.
pub(crate) fn check_unsigned_bitfield_overflow(
    value: u64,
    incr: i64,
    bits: u64,
    owtype: OverflowType,
) -> (i32, u64) {
    let max: u64 = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    // Protocol limits unsigned to ≤63 bits, so value ≤ max < 2^63 → safe i64 cast.
    let maxincr = (max - value) as i64;
    let minincr = -(value as i64);

    let compute_wrap = || -> u64 {
        // C: `handle_wrap` block — mask off bits above `bits`.
        let mask: u64 = if bits == 64 { 0 } else { u64::MAX << bits };
        value.wrapping_add(incr as u64) & !mask
    };

    if value > max || (incr > 0 && incr > maxincr) {
        let limit = match owtype {
            OverflowType::Wrap => compute_wrap(),
            OverflowType::Sat => max,
            OverflowType::Fail => 0,
        };
        (1, limit)
    } else if incr < 0 && incr < minincr {
        let limit = match owtype {
            OverflowType::Wrap => compute_wrap(),
            OverflowType::Sat => 0,
            OverflowType::Fail => 0,
        };
        (-1, limit)
    } else {
        (0, 0)
    }
}

/// Check signed bitfield overflow.
///
/// Returns `(direction, limit)` analogous to `check_unsigned_bitfield_overflow`.
///
/// C: `checkSignedBitfieldOverflow` — uses `goto handle_wrap:`.
pub(crate) fn check_signed_bitfield_overflow(
    value: i64,
    incr: i64,
    bits: u64,
    owtype: OverflowType,
) -> (i32, i64) {
    let max: i64 = if bits == 64 { i64::MAX } else { (1i64 << (bits - 1)) - 1 };
    let min: i64 = (-max) - 1;

    // C: cast to u64 to avoid signed overflow UB in the subtraction.
    let maxincr = ((max as u64).wrapping_sub(value as u64)) as i64;
    let minincr = ((min as u64).wrapping_sub(value as u64)) as i64;

    let compute_wrap = || -> i64 {
        // C: `handle_wrap` block — wraps using unsigned arithmetic, then sign-extends.
        let msb = 1u64 << (bits - 1);
        let c = (value as u64).wrapping_add(incr as u64);
        let c = if bits < 64 {
            let mask = u64::MAX << bits;
            if c & msb != 0 { c | mask } else { c & !mask }
        } else {
            c
        };
        c as i64
    };

    let overflowed = value > max
        || (bits != 64 && incr > maxincr)
        || (value >= 0 && incr > 0 && incr > maxincr);
    let underflowed = value < min
        || (bits != 64 && incr < minincr)
        || (value < 0 && incr < 0 && incr < minincr);

    if overflowed {
        let limit = match owtype {
            OverflowType::Wrap => compute_wrap(),
            OverflowType::Sat => max,
            OverflowType::Fail => 0,
        };
        (1, limit)
    } else if underflowed {
        let limit = match owtype {
            OverflowType::Wrap => compute_wrap(),
            OverflowType::Sat => min,
            OverflowType::Fail => 0,
        };
        (-1, limit)
    } else {
        (0, 0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §4  Argument-parsing helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a decimal integer from an ASCII byte slice.
///
/// Minimal local equivalent of C's `string2ll`.  Handles optional leading `-`;
/// no leading-zero stripping beyond what `checked_mul` / `checked_add` provide.
/// TODO(port): `string2ll` has additional edge-case handling (e.g. `-0`, very
/// large numbers); validate against the C implementation in Phase B.
fn parse_i64_from_bytes(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let (neg, digits) = if bytes[0] == b'-' { (true, &bytes[1..]) } else { (false, bytes) };
    if digits.is_empty() {
        return None;
    }
    let mut val: i64 = 0;
    for &b in digits {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    if neg { val.checked_neg() } else { Some(val) }
}

/// Parse a bit offset from a command argument byte slice.
///
/// Handles the `#<offset>` BITFIELD hash form when `hash` is `true` and `bits > 0`.
/// Applies the server's 512 MB limit unless `must_obey` is `true`.
///
/// C: `getBitOffsetFromArgument(client *c, robj *o, uint64_t *offset, int hash, int bits)`
///
/// TODO(architect): `must_obey` should come from `CommandContext::must_obey_client()`.
/// TODO(architect): 512 MB limit should come from `CommandContext::proto_max_bulk_len()`.
fn get_bit_offset_from_arg(arg: &[u8], hash: bool, bits: i32, must_obey: bool) -> Result<u64, RedisError> {
    const ERR: &[u8] = b"bit offset is not an integer or out of range";
    // Hard-coded proto_max_bulk_len: 512 MB (Valkey default).
    const PROTO_MAX_BULK_LEN: u64 = 512 * 1024 * 1024;

    let use_hash = arg.first() == Some(&b'#') && hash && bits > 0;
    let slice = if use_hash { &arg[1..] } else { arg };

    let mut loffset = parse_i64_from_bytes(slice).ok_or_else(|| RedisError::runtime(ERR))?;

    if use_hash {
        loffset = loffset
            .checked_mul(bits as i64)
            .ok_or_else(|| RedisError::runtime(ERR))?;
    }

    if loffset < 0 || (!must_obey && (loffset >> 3) as u64 >= PROTO_MAX_BULK_LEN) {
        return Err(RedisError::runtime(ERR));
    }

    Ok(loffset as u64)
}

/// Parse a BITFIELD type specifier (`u<N>` or `i<N>`) from a byte slice.
///
/// Returns `(is_signed, bit_width)` on success.
///
/// C: `getBitfieldTypeFromArgument(client *c, robj *o, int *sign, int *bits)`
fn get_bitfield_type_from_arg(arg: &[u8]) -> Result<(bool, u32), RedisError> {
    const ERR: &[u8] =
        b"Invalid bitfield type. Use something like i16 u8. Note that u64 is not supported but i64 is.";

    let sign = match arg.first() {
        Some(b'i') => true,
        Some(b'u') => false,
        _ => return Err(RedisError::runtime(ERR)),
    };

    let llbits = parse_i64_from_bytes(&arg[1..]).ok_or_else(|| RedisError::runtime(ERR))?;

    if llbits < 1 || (sign && llbits > 64) || (!sign && llbits > 63) {
        return Err(RedisError::runtime(ERR));
    }

    Ok((sign, llbits as u32))
}

// ─────────────────────────────────────────────────────────────────────────────
// §5  Key-lookup helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Return a read-only byte copy for a string `RedisObject`.
///
/// C: `getObjectReadOnlyString(robj *o, long *len, char *llbuf)` — uses a
/// caller-supplied stack buffer for integer-encoded objects.  Rust heap-allocates
/// because we cannot return a reference to a temporary.
///
/// TODO(port): integer-encoded objects (`OBJ_ENCODING_INT`) are not yet handled;
/// Phase B must match the `RedisObject::String(IntEncoded(_))` variant and
/// convert to decimal bytes, mirroring `ll2string` in C.
///
/// TODO(port): extra `Vec` allocation per call; Phase B can use `Cow<'_, [u8]>`
/// for the SDS case to avoid it.
///
/// PORT NOTE: kept as a local helper but **not** called from command bodies to
/// avoid type-inference confusion when `RedisObject` / `RedisError` are unresolved.
/// Command bodies inline the match directly (same pattern as `string.rs`).
#[allow(dead_code)]
fn get_object_readonly_bytes(obj: Option<&RedisObject>) -> Result<Option<Vec<u8>>, RedisError> {
    match obj {
        None => Ok(None),
        Some(o) => match o.as_string_bytes() {
            Some(b) => Ok(Some(b.to_vec())),
            None => Err(RedisError::wrong_type()),
        },
    }
}

/// Look up a key for a bit-write command, creating or zero-extending the value
/// so that `maxbit` is addressable.  Returns `(bytes, dirty)` where `dirty`
/// indicates the backing store changed length.
///
/// C: `lookupStringForBitCommand(client *c, uint64_t maxbit, int *dirty)`
///
/// TODO(port): returning `&mut [u8]` into `ctx`'s DB while `ctx` is also needed
/// for replies is a borrow-checker conflict; Phase B must refactor via a
/// staging-buffer approach or split `CommandContext` accessors.
fn lookup_string_for_bit_command(
    ctx: &mut CommandContext,
    maxbit: u64,
) -> Result<(Vec<u8>, bool), RedisError> {
    let min_len = ((maxbit >> 3) + 1) as usize;
    let key = ctx.arg(1)?.clone();

    // TODO(port): ctx.db_mut().lookup_key_write(key) — blocked on Phase 3 db_mut().
    match ctx.db().lookup_key_read(key.as_bytes()) {
        None => {
            // Create a zero-filled string of the required length.
            let bytes = vec![0u8; min_len];
            // TODO(port): ctx.db_mut().add(key, RedisObject::String(RedisString::from_bytes(&bytes)));
            Ok((bytes, true))
        }
        Some(obj) => match obj.as_string_bytes() {
            Some(s) => {
                let mut bytes = s.to_vec();
                let old_len = bytes.len();
                if bytes.len() < min_len {
                    bytes.resize(min_len, 0u8);
                }
                let dirty = bytes.len() != old_len;
                // TODO(port): write back grown bytes to ctx.db_mut().
                Ok((bytes, dirty))
            }
            None => Err(RedisError::wrong_type()),
        },
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §6  SETBIT command
// ─────────────────────────────────────────────────────────────────────────────

/// SETBIT key offset bitvalue
///
/// C: `setbitCommand(client *c)` in `bitops.c:689-731`.
pub fn setbit_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    let offset_arg = ctx.arg(2)?.clone();
    // TODO(architect): pass must_obey_client() for the 512 MB bypass.
    let bitoffset = get_bit_offset_from_arg(offset_arg.as_bytes(), false, 0, false)?;

    let val_arg = ctx.arg(3)?.clone();
    let on = parse_i64_from_bytes(val_arg.as_bytes())
        .ok_or_else(|| RedisError::runtime(b"bit is not an integer or out of range"))?;

    if on & !1 != 0 {
        return Err(RedisError::runtime(b"bit is not an integer or out of range"));
    }
    let on = on != 0;

    // TODO(port): borrow conflict — `bytes` mutably borrows ctx's DB while
    // `ctx.reply_integer` also borrows ctx; Phase B refactor needed.
    let (mut bytes, dirty) = lookup_string_for_bit_command(ctx, bitoffset)?;

    let byte_idx = (bitoffset >> 3) as usize;
    let bit_shift = 7 - (bitoffset & 0x7);
    let byteval = bytes[byte_idx];
    let bitval = (byteval >> bit_shift) & 1 != 0;

    if dirty || bitval != on {
        let new_byte = (byteval & !(1u8 << bit_shift)) | (if on { 1u8 << bit_shift } else { 0 });
        bytes[byte_idx] = new_byte;
        // TODO(port): write bytes back to DB via ctx.db_mut().
        // TODO(architect): ctx.signal_modified_key(&key);
        // TODO(architect): ctx.notify_keyspace_event(NOTIFY_STRING, b"setbit", &key);
        // TODO(architect): ctx.mark_dirty(1);
    }

    ctx.reply_integer(if bitval { 1 } else { 0 })
}

// ─────────────────────────────────────────────────────────────────────────────
// §7  GETBIT command
// ─────────────────────────────────────────────────────────────────────────────

/// GETBIT key offset
///
/// C: `getbitCommand(client *c)` in `bitops.c:733-754`.
pub fn getbit_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    let offset_arg = ctx.arg(2)?.clone();
    // TODO(architect): pass must_obey_client() for the 512 MB bypass.
    let bitoffset = get_bit_offset_from_arg(offset_arg.as_bytes(), false, 0, false)?;

    let key = ctx.arg(1)?.clone();
    let bytes: Vec<u8> = match ctx.db().lookup_key_read(key.as_bytes()) {
        None => return ctx.reply_integer(0),
        Some(o) => match o.as_string_bytes() {
            Some(s) => s.to_vec(),
            None => return Err(RedisError::wrong_type()),
        },
    };

    let byte_idx = (bitoffset >> 3) as usize;
    let bit_shift = 7 - (bitoffset & 0x7);
    let bitval = if byte_idx < bytes.len() {
        (bytes[byte_idx] >> bit_shift) & 1
    } else {
        0
    };

    ctx.reply_integer(bitval as i64)
}

// ─────────────────────────────────────────────────────────────────────────────
// §8  BITOP command
// ─────────────────────────────────────────────────────────────────────────────

/// BITOP op_name target_key src_key1 [src_key2 …]
///
/// C: `bitopCommand(client *c)` in `bitops.c:757-941`.
///
/// PERF(port): the C version has a fast path using word-aligned `unsigned long *`
/// reads (processing 4 words = 32 bytes per inner iteration).  That path requires
/// unsafe pointer casts; it is omitted here.  Profile in Phase B and introduce a
/// safe word-slice approach if warranted.
pub fn bitop_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    let op_arg = ctx.arg(1)?.clone();
    let op_bytes = op_arg.as_bytes();

    let op = match op_bytes {
        b if b.eq_ignore_ascii_case(b"and") => BitOp::And,
        b if b.eq_ignore_ascii_case(b"or")  => BitOp::Or,
        b if b.eq_ignore_ascii_case(b"xor") => BitOp::Xor,
        b if b.eq_ignore_ascii_case(b"not") => BitOp::Not,
        _ => return Err(RedisError::syntax(b"syntax error")),
    };

    let argc = ctx.argc();
    if op == BitOp::Not && argc != 4 {
        return Err(RedisError::runtime(b"BITOP NOT must be called with a single source key."));
    }

    let numkeys = argc - 3;
    let mut sources: Vec<Option<Vec<u8>>> = Vec::with_capacity(numkeys);
    let mut maxlen: usize = 0;
    let mut minlen: usize = usize::MAX;

    for j in 0..numkeys {
        let src_key = ctx.arg(j + 3)?.clone();
        match ctx.db().lookup_key_read(src_key.as_bytes()) {
            None => {
                sources.push(None);
                minlen = 0;
            }
            Some(o) => match o.as_string_bytes() {
                Some(s) => {
                    let bytes = s.to_vec();
                    if bytes.len() > maxlen {
                        maxlen = bytes.len();
                    }
                    if j == 0 || bytes.len() < minlen {
                        minlen = bytes.len();
                    }
                    sources.push(Some(bytes));
                }
                None => return Err(RedisError::wrong_type()),
            },
        }
    }

    let result_bytes: Option<Vec<u8>> = if maxlen > 0 {
        let mut res = vec![0u8; maxlen];

        // Byte-by-byte operation (covers all positions 0..maxlen).
        for pos in 0..maxlen {
            let first = sources[0].as_deref().and_then(|s| s.get(pos)).copied().unwrap_or(0);
            let mut output = if op == BitOp::Not { !first } else { first };

            for i in 1..numkeys {
                let byte = sources[i].as_deref().and_then(|s| s.get(pos)).copied().unwrap_or(0);
                match op {
                    BitOp::And => {
                        output &= byte;
                        if output == 0 {
                            break;
                        }
                    }
                    BitOp::Or => {
                        output |= byte;
                        if output == 0xFF {
                            break;
                        }
                    }
                    BitOp::Xor => output ^= byte,
                    BitOp::Not => {} // single source only
                }
            }
            res[pos] = output;
        }
        Some(res)
    } else {
        None
    };

    let target_key = ctx.arg(2)?.clone();

    // TODO(port): write result_bytes to DB via ctx.db_mut().
    if let Some(bytes) = result_bytes {
        // TODO(architect): ctx.db_mut().set_key(&target_key, RedisObject::String(RedisString::from_bytes(&bytes)));
        // TODO(architect): ctx.notify_keyspace_event(NOTIFY_STRING, b"set", &target_key);
        // TODO(architect): ctx.mark_dirty(1);
        let _ = bytes; // suppress unused warning in Phase A
        ctx.reply_integer(maxlen as i64)
    } else {
        // No result: delete target key if it existed.
        // TODO(architect): let deleted = ctx.db_mut().delete(&target_key);
        // if deleted { ctx.signal_modified_key(&target_key); ... }
        ctx.reply_integer(0)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §9  BITCOUNT command
// ─────────────────────────────────────────────────────────────────────────────

/// BITCOUNT key [start [end [BIT|BYTE]]]
///
/// C: `bitcountCommand(client *c)` in `bitops.c:943-1038`.
pub fn bitcount_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    let argc = ctx.argc();
    let key = ctx.arg(1)?.clone();

    let mut start: i64;
    let mut end: i64;
    let mut isbit = false;
    let mut first_byte_neg_mask: u8 = 0;
    let mut last_byte_neg_mask: u8 = 0;

    let obj = ctx.db().lookup_key_read(key.as_bytes());
    // Validate type early (checkType).
    if let Some(o) = obj {
        if !o.is_string() {
            return Err(RedisError::wrong_type());
        }
    }

    if argc == 2 {
        // Whole string.
        let bytes: Vec<u8> = match ctx.db().lookup_key_read(key.as_bytes()) {
            None => return ctx.reply_integer(0),
            Some(o) => match o.as_string_bytes() {
                Some(s) => s.to_vec(),
                None => return Err(RedisError::wrong_type()),
            },
        };
        let count = server_popcount(&bytes);
        return ctx.reply_integer(count);
    }

    if argc == 3 || argc == 4 || argc == 5 {
        let start_arg = ctx.arg(2)?.clone();
        start = parse_i64_from_bytes(start_arg.as_bytes())
            .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;

        if argc == 5 {
            let unit_arg = ctx.arg(4)?.clone();
            let unit = unit_arg.as_bytes();
            if unit.eq_ignore_ascii_case(b"bit") {
                isbit = true;
            } else if unit.eq_ignore_ascii_case(b"byte") {
                isbit = false;
            } else {
                return Err(RedisError::syntax(b"syntax error"));
            }
        }

        if argc >= 4 {
            let end_arg = ctx.arg(3)?.clone();
            end = parse_i64_from_bytes(end_arg.as_bytes())
                .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;
        } else {
            end = i64::MAX; // will be clamped below
        }

        let bytes: Vec<u8> = match ctx.db().lookup_key_read(key.as_bytes()) {
            None => return ctx.reply_integer(0),
            Some(o) => match o.as_string_bytes() {
                Some(s) => s.to_vec(),
                None => return Err(RedisError::wrong_type()),
            },
        };

        let strlen = bytes.len() as i64;
        debug_assert!(strlen <= i64::MAX >> 3, "string too large for bit arithmetic");
        let mut totlen = strlen;

        if argc < 4 {
            end = totlen - 1;
        }

        // Early return for negative range shortcut (C: start > end with both negative).
        if start < 0 && end < 0 && start > end {
            return ctx.reply_integer(0);
        }

        if isbit {
            totlen <<= 3;
        }
        if start < 0 { start = totlen + start; }
        if end < 0   { end   = totlen + end;   }
        if start < 0 { start = 0; }
        if end < 0   { end   = 0; }
        if end >= totlen { end = totlen - 1; }

        if isbit && start <= end {
            // Create masks to exclude bits at the byte edges that are out of range.
            first_byte_neg_mask = !((1u8 << (8 - (start & 7))) - 1) & 0xFF;
            last_byte_neg_mask  = (1u8 << (7 - (end & 7))).wrapping_sub(1);
            start >>= 3;
            end   >>= 3;
        }

        if start > end {
            return ctx.reply_integer(0);
        }

        let byte_start = start as usize;
        let byte_end = end as usize;
        let region = &bytes[byte_start..=byte_end];
        let mut count = server_popcount(region);

        if first_byte_neg_mask != 0 || last_byte_neg_mask != 0 {
            // Subtract out-of-range bits at the edges.
            let edge_bytes = [
                if first_byte_neg_mask != 0 { bytes[byte_start] & first_byte_neg_mask } else { 0 },
                if last_byte_neg_mask  != 0 { bytes[byte_end]   & last_byte_neg_mask  } else { 0 },
            ];
            count -= server_popcount(&edge_bytes);
        }

        ctx.reply_integer(count)
    } else {
        Err(RedisError::syntax(b"syntax error"))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// §10  BITPOS command
// ─────────────────────────────────────────────────────────────────────────────

/// BITPOS key bit [start [end [BIT|BYTE]]]
///
/// C: `bitposCommand(client *c)` in `bitops.c:1041-1181`.
/// The C function uses `goto result:` for early exit from the position search;
/// translated here as a Rust labeled block (`'find_pos`).
pub fn bitpos_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    let bit_arg = ctx.arg(2)?.clone();
    let bit = parse_i64_from_bytes(bit_arg.as_bytes())
        .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))? as i32;

    if bit != 0 && bit != 1 {
        return Err(RedisError::runtime(b"The bit argument must be 1 or 0."));
    }

    let argc = ctx.argc();
    let key = ctx.arg(1)?.clone();

    let mut start: i64 = 0;
    let mut end: i64 = 0;
    let mut end_given = false;
    let mut isbit = false;
    let mut first_byte_neg_mask: u8 = 0;
    let mut last_byte_neg_mask: u8 = 0;

    let bytes_opt: Vec<u8> = match ctx.db().lookup_key_read(key.as_bytes()) {
        None => {
            // Key does not exist: treat as infinite zero-bit string.
            return ctx.reply_integer(if bit == 1 { -1 } else { 0 });
        }
        Some(o) => match o.as_string_bytes() {
            Some(s) => s.to_vec(),
            None => return Err(RedisError::wrong_type()),
        },
    };
    let strlen = bytes_opt.len() as i64;
    debug_assert!(strlen <= i64::MAX >> 3, "string too large for bit arithmetic");

    if argc == 4 || argc == 5 || argc == 6 {
        let start_arg = ctx.arg(3)?.clone();
        start = parse_i64_from_bytes(start_arg.as_bytes())
            .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;

        if argc == 6 {
            let unit_arg = ctx.arg(5)?.clone();
            let unit = unit_arg.as_bytes();
            if unit.eq_ignore_ascii_case(b"bit") {
                isbit = true;
            } else if unit.eq_ignore_ascii_case(b"byte") {
                isbit = false;
            } else {
                return Err(RedisError::syntax(b"syntax error"));
            }
        }

        if argc >= 5 {
            let end_arg = ctx.arg(4)?.clone();
            end = parse_i64_from_bytes(end_arg.as_bytes())
                .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;
            end_given = true;
        } else {
            end = strlen - 1;
        }

        let mut totlen = strlen;
        if isbit { totlen <<= 3; }
        if start < 0 { start = totlen + start; }
        if end < 0   { end   = totlen + end;   }
        if start < 0 { start = 0; }
        if end < 0   { end   = 0; }
        if end >= totlen { end = totlen - 1; }

        if isbit && start <= end {
            first_byte_neg_mask = !((1u8 << (8 - (start & 7))) - 1) & 0xFF;
            last_byte_neg_mask  = (1u8 << (7 - (end & 7))).wrapping_sub(1);
            start >>= 3;
            end   >>= 3;
        }
    } else if argc == 3 {
        start = 0;
        end = strlen - 1;
    } else {
        return Err(RedisError::syntax(b"syntax error"));
    }

    if start > end {
        return ctx.reply_integer(-1);
    }

    let p = &bytes_opt;
    let mut search_start = start;
    let mut bytes = end - start + 1;

    // C: `bitposCommand` uses `goto result:` to skip straight to the
    // post-search adjustment.  Translated as a labeled block.
    let pos: i64 = 'find_pos: {
        if first_byte_neg_mask != 0 {
            let mut tmpchar = if bit == 1 {
                p[search_start as usize] & !first_byte_neg_mask
            } else {
                p[search_start as usize] | first_byte_neg_mask
            };
            // Special case: only one byte in the range.
            if last_byte_neg_mask != 0 && bytes == 1 {
                tmpchar = if bit == 1 {
                    tmpchar & !last_byte_neg_mask
                } else {
                    tmpchar | last_byte_neg_mask
                };
            }
            let pos = server_bitpos(&[tmpchar], 1, bit);
            if bytes == 1 || (pos != -1 && pos != 8) {
                break 'find_pos pos;
            }
            search_start += 1;
            bytes -= 1;
        }

        let curbytes = bytes - if last_byte_neg_mask != 0 { 1 } else { 0 };
        if curbytes > 0 {
            let slice = &p[search_start as usize..(search_start + curbytes) as usize];
            let pos = server_bitpos(slice, curbytes as usize, bit);
            if bytes == curbytes || (pos != -1 && pos != (curbytes as i64) << 3) {
                break 'find_pos pos;
            }
            search_start += curbytes;
            bytes -= curbytes;
        }

        let tmpchar = if bit == 1 {
            p[end as usize] & !last_byte_neg_mask
        } else {
            p[end as usize] | last_byte_neg_mask
        };
        server_bitpos(&[tmpchar], 1, bit)
    };

    // Post-search: if no set bit found for 0-search with explicit end, return -1.
    if end_given && bit == 0 && pos == (bytes as i64) << 3 {
        return ctx.reply_integer(-1);
    }

    let final_pos = if pos != -1 {
        pos + (search_start << 3)
    } else {
        -1
    };
    ctx.reply_integer(final_pos)
}

// ─────────────────────────────────────────────────────────────────────────────
// §11  BITFIELD / BITFIELD_RO
// ─────────────────────────────────────────────────────────────────────────────

/// Core implementation for BITFIELD and BITFIELD_RO.
///
/// C: `bitfieldGeneric(client *c, int flags)` in `bitops.c:1208-1423`.
///
/// When `readonly` is `true`, only GET subcommands are permitted (BITFIELD_RO).
fn bitfield_generic(ctx: &mut CommandContext, readonly: bool) -> Result<(), RedisError> {
    let argc = ctx.argc();
    let mut ops: Vec<BitfieldOp> = Vec::new();
    let mut owtype = OverflowType::Wrap;
    let mut is_readonly_ops = true;
    let mut highest_write_offset: u64 = 0;

    let mut j = 2usize;
    while j < argc {
        let remargs = argc - j - 1;
        let subcmd_arg = ctx.arg(j)?.clone();
        let subcmd = subcmd_arg.as_bytes();

        if subcmd.eq_ignore_ascii_case(b"get") && remargs >= 2 {
            // GET <type> <offset>
            let type_arg = ctx.arg(j + 1)?.clone();
            let (sign, bits) = get_bitfield_type_from_arg(type_arg.as_bytes())?;
            let off_arg = ctx.arg(j + 2)?.clone();
            let offset = get_bit_offset_from_arg(off_arg.as_bytes(), true, bits as i32, false)?;

            ops.push(BitfieldOp {
                offset,
                i64: 0,
                opcode: BitfieldOpCode::Get,
                owtype,
                bits,
                sign,
            });
            j += 3;
        } else if subcmd.eq_ignore_ascii_case(b"set") && remargs >= 3 {
            if readonly {
                return Err(RedisError::runtime(b"BITFIELD_RO only supports the GET subcommand"));
            }
            let type_arg = ctx.arg(j + 1)?.clone();
            let (sign, bits) = get_bitfield_type_from_arg(type_arg.as_bytes())?;
            let off_arg = ctx.arg(j + 2)?.clone();
            let offset = get_bit_offset_from_arg(off_arg.as_bytes(), true, bits as i32, false)?;
            let val_arg = ctx.arg(j + 3)?.clone();
            let i64_val = parse_i64_from_bytes(val_arg.as_bytes())
                .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;

            is_readonly_ops = false;
            if highest_write_offset < offset + bits as u64 - 1 {
                highest_write_offset = offset + bits as u64 - 1;
            }
            ops.push(BitfieldOp { offset, i64: i64_val, opcode: BitfieldOpCode::Set, owtype, bits, sign });
            j += 4;
        } else if subcmd.eq_ignore_ascii_case(b"incrby") && remargs >= 3 {
            if readonly {
                return Err(RedisError::runtime(b"BITFIELD_RO only supports the GET subcommand"));
            }
            let type_arg = ctx.arg(j + 1)?.clone();
            let (sign, bits) = get_bitfield_type_from_arg(type_arg.as_bytes())?;
            let off_arg = ctx.arg(j + 2)?.clone();
            let offset = get_bit_offset_from_arg(off_arg.as_bytes(), true, bits as i32, false)?;
            let incr_arg = ctx.arg(j + 3)?.clone();
            let i64_val = parse_i64_from_bytes(incr_arg.as_bytes())
                .ok_or_else(|| RedisError::runtime(b"value is not an integer or out of range"))?;

            is_readonly_ops = false;
            if highest_write_offset < offset + bits as u64 - 1 {
                highest_write_offset = offset + bits as u64 - 1;
            }
            ops.push(BitfieldOp { offset, i64: i64_val, opcode: BitfieldOpCode::IncrBy, owtype, bits, sign });
            j += 4;
        } else if subcmd.eq_ignore_ascii_case(b"overflow") && remargs >= 1 {
            let ow_arg = ctx.arg(j + 1)?.clone();
            let ow = ow_arg.as_bytes();
            owtype = if ow.eq_ignore_ascii_case(b"wrap") {
                OverflowType::Wrap
            } else if ow.eq_ignore_ascii_case(b"sat") {
                OverflowType::Sat
            } else if ow.eq_ignore_ascii_case(b"fail") {
                OverflowType::Fail
            } else {
                return Err(RedisError::runtime(b"Invalid OVERFLOW type specified"));
            };
            j += 2;
        } else {
            return Err(RedisError::syntax(b"syntax error"));
        }
    }

    // Fetch or create the backing byte buffer.
    // PORT NOTE: `bytes` is always `Vec<u8>` (empty = key absent or zero-length);
    // this flattens the C's NULL-vs-robj distinction for simpler Rust ownership.
    // TODO(port): borrow conflict between `bytes` and later `ctx.reply_*` calls;
    // Phase B must use a staging buffer or split CommandContext accessors.
    let key = ctx.arg(1)?.clone();
    let mut bytes: Vec<u8> = if is_readonly_ops {
        match ctx.db().lookup_key_read(key.as_bytes()) {
            None => Vec::new(),
            Some(o) => match o.as_string_bytes() {
                Some(s) => s.to_vec(),
                None => return Err(RedisError::wrong_type()),
            },
        }
    } else {
        let (b, _dirty) = lookup_string_for_bit_command(ctx, highest_write_offset)?;
        b
    };

    // TODO(architect): ctx.begin_deferred_array();
    // TODO(architect): ctx.reply_array_header(ops.len());

    let numops = ops.len();
    let mut changes = 0usize;
    let dirty = false; // TODO(port): real dirty flag from lookup_string_for_bit_command

    for op in &ops {
        // C: bitops.c:1313-1413
        if op.opcode == BitfieldOpCode::Get {
            // Copy up to 9 bytes around the target offset for safe 64-bit access.
            let mut buf = [0u8; 9];
            let byte_offset = (op.offset >> 3) as usize;
            for (i, slot) in buf.iter_mut().enumerate() {
                let src_idx = byte_offset + i;
                if src_idx < bytes.len() {
                    *slot = bytes[src_idx];
                }
            }
            let local_offset = op.offset - (byte_offset as u64 * 8);
            let val: i64 = if op.sign {
                get_signed_bitfield(&buf, local_offset, op.bits as u64)
            } else {
                get_unsigned_bitfield(&buf, local_offset, op.bits as u64) as i64
            };
            ctx.reply_integer(val)?;
        } else {
            // SET or INCRBY: bytes is always present (created by lookup_string_for_bit_command).
            if op.sign {
                let oldval = get_signed_bitfield(&bytes, op.offset, op.bits as u64);
                let (newval, retval) = if op.opcode == BitfieldOpCode::IncrBy {
                    let (overflow, wrapped) =
                        check_signed_bitfield_overflow(oldval, op.i64, op.bits as u64, op.owtype);
                    let nv = if overflow != 0 { wrapped } else { oldval + op.i64 };
                    (nv, nv)
                } else {
                    let nv = op.i64;
                    let (overflow, wrapped) =
                        check_signed_bitfield_overflow(nv, 0, op.bits as u64, op.owtype);
                    let nv = if overflow != 0 { wrapped } else { nv };
                    (nv, oldval)
                };

                let (overflow, _) =
                    check_signed_bitfield_overflow(oldval, op.i64, op.bits as u64, op.owtype);
                if !(overflow != 0 && op.owtype == OverflowType::Fail) {
                    ctx.reply_integer(retval)?;
                    set_signed_bitfield(&mut bytes, op.offset, op.bits as u64, newval);
                    if dirty || oldval != newval {
                        changes += 1;
                    }
                } else {
                    ctx.reply_null()?;
                }
            } else {
                let oldval = get_unsigned_bitfield(&bytes, op.offset, op.bits as u64);
                let (newval, retval) = if op.opcode == BitfieldOpCode::IncrBy {
                    let raw = oldval.wrapping_add(op.i64 as u64);
                    let (overflow, wrapped) =
                        check_unsigned_bitfield_overflow(oldval, op.i64, op.bits as u64, op.owtype);
                    let nv = if overflow != 0 { wrapped } else { raw };
                    (nv, nv)
                } else {
                    let nv = op.i64 as u64;
                    let (overflow, wrapped) =
                        check_unsigned_bitfield_overflow(nv, 0, op.bits as u64, op.owtype);
                    let nv = if overflow != 0 { wrapped } else { nv };
                    (nv, oldval)
                };

                let (overflow, _) =
                    check_unsigned_bitfield_overflow(oldval, op.i64, op.bits as u64, op.owtype);
                if !(overflow != 0 && op.owtype == OverflowType::Fail) {
                    ctx.reply_integer(retval as i64)?;
                    set_unsigned_bitfield(&mut bytes, op.offset, op.bits as u64, newval);
                    if dirty || oldval != newval {
                        changes += 1;
                    }
                } else {
                    ctx.reply_null()?;
                }
            }
        }
    }

    if changes > 0 {
        // TODO(port): write back modified bytes to DB.
        // TODO(architect): ctx.signal_modified_key(&key);
        // TODO(architect): ctx.notify_keyspace_event(NOTIFY_STRING, b"setbit", &key);
        // TODO(architect): ctx.mark_dirty(changes);
    }

    // TODO(architect): ctx.commit_deferred_array();
    let _ = numops; // suppress unused warning
    Ok(())
}

/// BITFIELD key [GET type offset] [SET type offset value] [INCRBY type offset increment]
///            [OVERFLOW WRAP|SAT|FAIL]
///
/// C: `bitfieldCommand(client *c)` in `bitops.c:1425-1427`.
pub fn bitfield_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    bitfield_generic(ctx, false)
}

/// BITFIELD_RO key [GET type offset]
///
/// C: `bitfieldroCommand(client *c)` in `bitops.c:1429-1431`.
pub fn bitfield_ro_command(ctx: &mut CommandContext) -> Result<(), RedisError> {
    bitfield_generic(ctx, true)
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/bitops.c  (1432 lines, ~25 functions)
//   target_crate:  redis-commands
//   confidence:    medium
//   todos:         41
//   port_notes:    2
//   unsafe_blocks: 0
//   notes:         Logic faithful; SIMD paths replaced by count_ones(); word-aligned
//                  fast paths in BITOP/bitpos omitted (5 PERF markers present).
//                  Borrow-checker conflicts between mutable DB access and ctx.reply_*
//                  are flagged TODO(port); Phase B must resolve via CommandContext
//                  split-borrow or staging-buffer.  goto→labeled-block translations
//                  are in bitpos_command and the overflow checkers.
//                  Validator (rustc --emit=metadata) shows only expected E0432/E0433
//                  name-resolution errors; no real syntax errors.
// ──────────────────────────────────────────────────────────────────────────
