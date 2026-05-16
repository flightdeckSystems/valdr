//! Call-reply parsing and representation.
//!
//! Port of `call_reply.c` (869 lines, ~46 functions) + `call_reply.h` (65 lines).
//!
//! Provides [`CallReply`], an owned RESP-frame tree that results from parsing a raw
//! RESP buffer captured during `RM_Call` or Lua scripting.  The type offers a
//! recursive-descent parser (via [`ParserCallbacks`] on [`CallReplyParserCtx`])
//! and a streaming handler-dispatch path ([`invoke_reply_handlers`]) for the
//! Valkey Module API.
//!
//! # Two-level callback architecture
//!
//! **Level 1 — [`ParserCallbacks`]** (from `redis-protocol`):
//! The low-level recursive-descent parser drives these callbacks during a
//! single pass over a raw RESP buffer.  Two implementations live here:
//! - [`CallReplyParserCtx`] — builds a [`CallReply`] node tree; used by
//!   [`CallReply::ensure_parsed`].
//! - [`RespHandlersParserCtx`] — adapts Level-1 callbacks to Level-2
//!   [`ValkeyModuleReplyHandlers`]; used by [`invoke_reply_handlers`].
//!
//! **Level 2 — [`ValkeyModuleReplyHandlers`]** (Phase 10 module API):
//! User-supplied callbacks receive only clean parsed values.  Collection
//! callbacks come in matched Start/End pairs so the module does not need to
//! drive the parser itself.
//!
//! C: `call_reply.c:38-73` (design comment)
//!
//! # Ownership model
//!
//! The root [`CallReply`] owns `original_proto` (the captured RESP bytes).
//! Sub-replies in the C code borrow into that buffer via raw pointers; in
//! Rust each node stores its proto slice as an owned `Vec<u8>` copy.
//!
//! PERF(port): C sub-replies are zero-copy slices into the root's buffer.
//! The Rust port copies bytes per node.  Profile in Phase B and consider
//! switching to `bytes::Bytes` (cheap clone via Arc) if the alloc pressure
//! is visible.

use std::any::Any;
use std::sync::Arc;

use redis_protocol::parser::{ParserCallbacks, ParserCursor};

// ── Internal flags ─────────────────────────────────────────────────────────
// C: call_reply.c:33-36

const REPLY_FLAG_ROOT: u32 = 1 << 0;
const REPLY_FLAG_PARSED: u32 = 1 << 1;
const REPLY_FLAG_RESP3: u32 = 1 << 2;
const REPLY_FLAG_EXACT_TYPE: u32 = 1 << 3;

// ── Reply type discriminant ────────────────────────────────────────────────
// Mirrors VALKEYMODULE_REPLY_* constants from valkeymodule.h:101-115.

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CallReplyType {
    #[default]
    Unknown = -1,
    String = 0,
    Error = 1,
    Integer = 2,
    Array = 3,
    Null = 4,
    Map = 5,
    Set = 6,
    Bool = 7,
    Double = 8,
    BigNumber = 9,
    VerbatimString = 10,
    Attribute = 11,
    Promise = 12,
    SimpleString = 13,
    ArrayNull = 14,
}

// ── Value payload (replaces the C union) ──────────────────────────────────
// C: call_reply.c:89-101 — `union { str; verbatim_str; ll; d; array }`
//
// PORT NOTE: In C, `str` and `verbatim_str.str/format` are non-owning pointers
// into the proto buffer.  In Rust they are owned `Vec<u8>` copies to avoid
// self-referential lifetimes.  See PERF note in the module docstring.

enum CallReplyValue {
    None,
    Str(Vec<u8>),
    VerbatimStr { data: Vec<u8>, format: Vec<u8> },
    LongLong(i64),
    Double(f64),
    Array(Vec<CallReply>),
}

impl Default for CallReplyValue {
    fn default() -> Self {
        CallReplyValue::None
    }
}

// ── Phase-10 module-API stubs ──────────────────────────────────────────────
// TODO(architect): ValkeyModuleReplyHandlers and ValkeyModuleCtx are owned by
// redis-modules (Phase 10, crates/redis-modules/src/api.rs).  These stubs let
// the Phase A draft capture the invoke_reply_handlers logic; replace with real
// imports when redis-modules is introduced.

/// Stub for the module context passed to `invokeReplyHandlers`.
/// C: `ValkeyModuleCtx *` in valkeymodule.h (Phase 10)
pub struct ValkeyModuleCtx;

/// Handler callbacks provided by a module for streaming reply dispatch.
/// C: `ValkeyModuleReplyHandlersV1` in valkeymodule.h:1376-1411
///
/// Every field is `Option<fn>` so modules only implement the callbacks they care
/// about.  `void *ctx` from C is replaced by a module-supplied `reply_ctx: *mut ()`
/// passed to each callback.
///
/// TODO(architect): migrate function-pointer fields to `Box<dyn Fn(...)>` or a
/// trait object when the full module ABI is designed in Phase 10.
pub struct ValkeyModuleReplyHandlers {
    pub null: Option<fn()>,
    pub null_bulk_string: Option<fn()>,
    pub null_array: Option<fn()>,
    pub bulk_string: Option<fn(data: &[u8])>,
    pub simple_string: Option<fn(data: &[u8])>,
    pub verbatim_string: Option<fn(data: &[u8], fmt: &[u8])>,
    pub integer: Option<fn(val: i64)>,
    pub double_val: Option<fn(val: f64)>,
    pub big_number: Option<fn(data: &[u8])>,
    pub bool_val: Option<fn(val: bool)>,
    pub attribute_start: Option<fn(len: usize)>,
    pub attribute_end: Option<fn()>,
    pub array_start: Option<fn(len: usize)>,
    pub array_end: Option<fn()>,
    pub map_start: Option<fn(len: usize)>,
    pub map_end: Option<fn()>,
    pub set_start: Option<fn(len: usize)>,
    pub set_end: Option<fn()>,
    pub reply_parsing_error: Option<fn()>,
    /// C: `onRespAvailable` — if set, called with raw RESP bytes before per-type dispatch.
    /// Returns `true` to continue per-type dispatch, `false` to skip it.
    pub on_resp_available: Option<fn(ctx: &ValkeyModuleCtx, proto: &[u8]) -> bool>,
}

// ── CallReply ──────────────────────────────────────────────────────────────
// C: call_reply.c:81-104 — `struct CallReply`
//
// PORT NOTE: The C struct is opaque (typedef'd in the header).  The Rust struct
// is public but its fields are private; access is via the method API below.

pub struct CallReply {
    /// Caller-supplied opaque handle; not freed by `CallReply`.
    /// C: `void *private_data`
    ///
    /// TODO(architect): `void *` is an un-owned raw pointer in C — the caller
    /// manages its lifetime.  `Arc<dyn Any + Send + Sync>` is the safe Rust
    /// equivalent but requires callers to wrap their data.  Decide the final
    /// ABI in Phase 10 when the module layer is introduced.
    private_data: Option<Arc<dyn Any + Send + Sync>>,

    /// Owned RESP buffer; present only on the root node.
    /// C: `sds original_proto`
    original_proto: Option<Vec<u8>>,

    /// Slice of raw RESP bytes that this node covers.
    /// C: `const char *proto` + `size_t proto_len`
    ///
    /// PORT NOTE: In C this is a non-owning pointer into `original_proto`.
    /// Here it is an owned copy per node.  See module-level PERF note.
    proto: Vec<u8>,

    /// RESP reply type discriminant.
    /// C: `int type`
    reply_type: CallReplyType,

    /// Bitmask of REPLY_FLAG_* constants.
    /// C: `int flags`
    flags: u32,

    /// Element count (for strings: byte length; for aggregates: entry count).
    /// C: `size_t len`
    len: usize,

    /// Payload union replacement.
    val: CallReplyValue,

    /// Errors accumulated during deferred parsing, if any.
    /// C: `list *deferred_error_list`  (adlist of sds)
    deferred_error_list: Option<Vec<Vec<u8>>>,

    /// RESP3 attribute metadata attached to this reply, if present.
    /// C: `struct CallReply *attribute`
    attribute: Option<Box<CallReply>>,
}

impl CallReply {
    /// Allocate a zeroed root `CallReply`.  Internal use only.
    fn new_root() -> Self {
        CallReply {
            private_data: None,
            original_proto: None,
            proto: Vec::new(),
            reply_type: CallReplyType::Unknown,
            flags: 0,
            len: 0,
            val: CallReplyValue::None,
            deferred_error_list: None,
            attribute: None,
        }
    }

    /// Allocate a zeroed child `CallReply` inheriting `private_data` from a parent.
    /// C: per-child initialisation in `callReplyParseCollection` (call_reply.c:200)
    fn new_child(private_data: Option<Arc<dyn Any + Send + Sync>>) -> Self {
        CallReply {
            private_data,
            original_proto: None,
            proto: Vec::new(),
            reply_type: CallReplyType::Unknown,
            flags: 0,
            len: 0,
            val: CallReplyValue::None,
            deferred_error_list: None,
            attribute: None,
        }
    }

    /// Create a root `CallReply` that owns the captured RESP bytes.
    ///
    /// The `deferred_error_list` is a list of pre-identified error frames (in sds
    /// form) already found in `reply`.  Ownership transfers to this `CallReply`.
    ///
    /// C: `callReplyCreate` (call_reply.c:589-599)
    pub fn create(
        reply: Vec<u8>,
        deferred_error_list: Option<Vec<Vec<u8>>>,
        private_data: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Box<CallReply> {
        let proto_copy = reply.clone();
        let mut res = Box::new(CallReply::new_root());
        res.flags = REPLY_FLAG_ROOT;
        res.proto = proto_copy;
        res.original_proto = Some(reply);
        res.private_data = private_data;
        res.deferred_error_list = deferred_error_list;
        res
    }

    /// Create a root `CallReply` representing an error message.
    ///
    /// If `reply` does not start with `-`, this function prepends `-ERR ` and
    /// appends `\r\n` to form a valid RESP error frame.  The `deferred_error_list`
    /// is populated with a copy of the resulting frame.
    ///
    /// C: `callReplyCreateError` (call_reply.c:607-617)
    pub fn create_error(
        reply: Vec<u8>,
        private_data: Option<Arc<dyn Any + Send + Sync>>,
    ) -> Box<CallReply> {
        let err_buf: Vec<u8> = if reply.first() == Some(&b'-') {
            reply
        } else {
            let mut buf = b"-ERR ".to_vec();
            buf.extend_from_slice(&reply);
            buf.extend_from_slice(b"\r\n");
            buf
        };
        let error_entry = err_buf.clone();
        let deferred = Some(vec![error_entry]);
        CallReply::create(err_buf, deferred, private_data)
    }

    /// Create a promise-type `CallReply` (used by async module calls).
    ///
    /// The returned reply has type `Promise`, is flagged as parsed and as root so
    /// that `freeCallReply` does not ignore it.
    ///
    /// C: `callReplyCreatePromise` (call_reply.c:300-308)
    pub fn create_promise(private_data: Option<Arc<dyn Any + Send + Sync>>) -> Box<CallReply> {
        let mut res = Box::new(CallReply::new_root());
        res.reply_type = CallReplyType::Promise;
        res.flags |= REPLY_FLAG_PARSED | REPLY_FLAG_ROOT;
        res.private_data = private_data;
        res
    }

    /// Enable exact reply-type parsing before the first access.
    ///
    /// When set, the parser preserves type distinctions that are normally
    /// collapsed (simple string vs bulk string; null array vs generic null).
    ///
    /// Must be called before any accessor that triggers lazy parsing.
    ///
    /// C: `enableParseExactReplyTypeFlag` (call_reply.c:625-628)
    pub fn enable_exact_type(&mut self) {
        debug_assert!(
            self.flags & REPLY_FLAG_PARSED == 0,
            "enable_exact_type called after the reply was already parsed"
        );
        self.flags |= REPLY_FLAG_EXACT_TYPE;
    }

    /// Parse the proto buffer on first access (lazy).
    ///
    /// C: `callReplyParse` (call_reply.c:338-347)
    fn ensure_parsed(&mut self) {
        if self.flags & REPLY_FLAG_PARSED != 0 {
            return;
        }
        let proto = self.proto.clone();
        let exact_type = self.flags & REPLY_FLAG_EXACT_TYPE != 0;
        {
            let mut cursor = ParserCursor::new(&proto);
            let mut ctx = CallReplyParserCtx::new(self, exact_type);
            let _ = cursor.parse_next(&mut ctx);
        }
        self.flags |= REPLY_FLAG_PARSED;
    }

    /// Return the reply type, triggering lazy parsing if needed.
    ///
    /// C: `callReplyType` (call_reply.c:350-354)
    pub fn get_type(&mut self) -> CallReplyType {
        self.ensure_parsed();
        self.reply_type
    }

    /// Return the string payload and its byte length, or `None` for non-string types.
    ///
    /// Applicable to: `String`, `SimpleString`, `Error`.
    ///
    /// The returned slice is valid for the lifetime of `self`.
    ///
    /// C: `callReplyGetString` (call_reply.c:367-374)
    pub fn get_string(&mut self) -> Option<&[u8]> {
        self.ensure_parsed();
        match self.reply_type {
            CallReplyType::String
            | CallReplyType::SimpleString
            | CallReplyType::Error => match &self.val {
                CallReplyValue::Str(s) => Some(s.as_slice()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Return the integer value, or `i64::MIN` if the reply is not `Integer`.
    ///
    /// C: `callReplyGetLongLong` (call_reply.c:379-383)
    pub fn get_long_long(&mut self) -> i64 {
        self.ensure_parsed();
        if self.reply_type != CallReplyType::Integer {
            return i64::MIN;
        }
        match self.val {
            CallReplyValue::LongLong(v) => v,
            _ => i64::MIN,
        }
    }

    /// Return the double value, or `i64::MIN as f64` if the reply is not `Double`.
    ///
    /// C: `callReplyGetDouble` (call_reply.c:388-392) — uses `LLONG_MIN` as sentinel.
    pub fn get_double(&mut self) -> f64 {
        self.ensure_parsed();
        if self.reply_type != CallReplyType::Double {
            return i64::MIN as f64;
        }
        match self.val {
            CallReplyValue::Double(d) => d,
            _ => i64::MIN as f64,
        }
    }

    /// Return the boolean value as `i32`, or `i32::MIN` if the reply is not `Bool`.
    ///
    /// C: `callReplyGetBool` (call_reply.c:397-401) — returns `INT_MIN` on mismatch.
    pub fn get_bool(&mut self) -> i32 {
        self.ensure_parsed();
        if self.reply_type != CallReplyType::Bool {
            return i32::MIN;
        }
        match self.val {
            CallReplyValue::LongLong(v) => v as i32,
            _ => i32::MIN,
        }
    }

    /// Return the element count (byte length for strings; entry count for aggregates).
    ///
    /// Applicable to: `String`, `Error`, `Array`, `Set`, `Map`, `Attribute`.
    /// Returns `0` for other types.
    ///
    /// C: `callReplyGetLen` (call_reply.c:411-422)
    pub fn get_len(&mut self) -> usize {
        self.ensure_parsed();
        match self.reply_type {
            CallReplyType::String
            | CallReplyType::Error
            | CallReplyType::Array
            | CallReplyType::Set
            | CallReplyType::Map
            | CallReplyType::Attribute => self.len,
            _ => 0,
        }
    }

    /// Return the element at flat index `idx` within a collection reply.
    ///
    /// For arrays and sets, `elements_per_entry = 1`.
    /// For maps and attributes, `elements_per_entry = 2` (key at `2*i`, value at `2*i+1`).
    ///
    /// Returns `None` if `idx >= len * elements_per_entry`.
    ///
    /// C: `callReplyGetCollectionElement` (call_reply.c:424-427)
    fn get_collection_element(&self, idx: usize, elements_per_entry: usize) -> Option<&CallReply> {
        let max_idx = self.len.saturating_mul(elements_per_entry);
        if idx >= max_idx {
            return None;
        }
        match &self.val {
            CallReplyValue::Array(arr) => arr.get(idx),
            _ => None,
        }
    }

    /// Mutable variant of `get_collection_element` for lazy-parsing child nodes.
    fn get_collection_element_mut(
        &mut self,
        idx: usize,
        elements_per_entry: usize,
    ) -> Option<&mut CallReply> {
        let max_idx = self.len.saturating_mul(elements_per_entry);
        if idx >= max_idx {
            return None;
        }
        match &mut self.val {
            CallReplyValue::Array(arr) => arr.get_mut(idx),
            _ => None,
        }
    }

    /// Return the array element at `idx`, or `None` if out of range or wrong type.
    ///
    /// C: `callReplyGetArrayElement` (call_reply.c:435-439)
    pub fn get_array_element(&mut self, idx: usize) -> Option<&mut CallReply> {
        self.ensure_parsed();
        if self.reply_type != CallReplyType::Array {
            return None;
        }
        self.get_collection_element_mut(idx, 1)
    }

    /// Return the set element at `idx`, or `None` if out of range or wrong type.
    ///
    /// C: `callReplyGetSetElement` (call_reply.c:447-451)
    pub fn get_set_element(&mut self, idx: usize) -> Option<&mut CallReply> {
        self.ensure_parsed();
        if self.reply_type != CallReplyType::Set {
            return None;
        }
        self.get_collection_element_mut(idx, 1)
    }

    /// Internal helper: retrieve a key–value pair from a map-like collection.
    ///
    /// C: `callReplyGetMapElementInternal` (call_reply.c:453-460)
    fn get_map_element_internal(
        &mut self,
        idx: usize,
        expected_type: CallReplyType,
    ) -> Result<(&mut CallReply, &mut CallReply), ()> {
        self.ensure_parsed();
        if self.reply_type != expected_type {
            return Err(());
        }
        if idx >= self.len {
            return Err(());
        }
        match &mut self.val {
            CallReplyValue::Array(arr) => {
                let key_idx = idx * 2;
                let val_idx = idx * 2 + 1;
                if val_idx >= arr.len() {
                    return Err(());
                }
                // Split borrow: get two mutable refs from one Vec.
                // TODO(port): split_at_mut is cleaner but needs index arithmetic.
                // For Phase A, returning tuple of indices and letting caller index.
                // This won't compile as-is (two mut borrows); Phase B will fix.
                let (left, right) = arr.split_at_mut(val_idx);
                Ok((&mut left[key_idx], &mut right[0]))
            }
            _ => Err(()),
        }
    }

    /// Retrieve a map entry key and value at `idx`.
    ///
    /// Returns `Ok((key, val))` or `Err(())` if the reply is not a `Map` or `idx`
    /// is out of range.
    ///
    /// C: `callReplyGetMapElement` (call_reply.c:474-476)
    pub fn get_map_element(
        &mut self,
        idx: usize,
    ) -> Result<(&mut CallReply, &mut CallReply), ()> {
        self.get_map_element_internal(idx, CallReplyType::Map)
    }

    /// Return the attribute sub-reply, or `None` if absent.
    ///
    /// C: `callReplyGetAttribute` (call_reply.c:483-485)
    pub fn get_attribute(&self) -> Option<&CallReply> {
        self.attribute.as_deref()
    }

    /// Retrieve an attribute entry key and value at `idx`.
    ///
    /// PORT NOTE: The C source passes `VALKEYMODULE_REPLY_MAP` (not
    /// `VALKEYMODULE_REPLY_ATTRIBUTE`) to `callReplyGetMapElementInternal`, so
    /// the check will always fail when called on an ATTRIBUTE-typed reply.  This
    /// appears to be a bug in the upstream C code.  Replicated faithfully here.
    ///
    /// C: `callReplyGetAttributeElement` (call_reply.c:499-501)
    pub fn get_attribute_element(
        &mut self,
        idx: usize,
    ) -> Result<(&mut CallReply, &mut CallReply), ()> {
        // PORT NOTE: upstream passes MAP type, not ATTRIBUTE — see above.
        self.get_map_element_internal(idx, CallReplyType::Map)
    }

    /// Return the big-number payload, or `None` if the reply is not `BigNumber`.
    ///
    /// C: `callReplyGetBigNumber` (call_reply.c:515-520)
    pub fn get_big_number(&mut self) -> Option<&[u8]> {
        self.ensure_parsed();
        if self.reply_type != CallReplyType::BigNumber {
            return None;
        }
        match &self.val {
            CallReplyValue::Str(s) => Some(s.as_slice()),
            _ => None,
        }
    }

    /// Return the verbatim string payload and format tag, or `None` if wrong type.
    ///
    /// `format` is the 3-byte type tag (e.g. `b"txt"` or `b"mkd"`).
    ///
    /// C: `callReplyGetVerbatim` (call_reply.c:536-542)
    pub fn get_verbatim(&mut self) -> Option<(&[u8], &[u8])> {
        self.ensure_parsed();
        if self.reply_type != CallReplyType::VerbatimString {
            return None;
        }
        match &self.val {
            CallReplyValue::VerbatimStr { data, format } => Some((data.as_slice(), format.as_slice())),
            _ => None,
        }
    }

    /// Return the raw RESP bytes for this reply node.
    ///
    /// C: `callReplyGetProto` (call_reply.c:549-552)
    pub fn get_proto(&self) -> &[u8] {
        &self.proto
    }

    /// Return the caller-supplied private data handle.
    ///
    /// C: `callReplyGetPrivateData` (call_reply.c:556-558)
    pub fn get_private_data(&self) -> Option<&Arc<dyn Any + Send + Sync>> {
        self.private_data.as_ref()
    }

    /// Return `true` if this reply or any sub-reply is RESP3 formatted.
    ///
    /// C: `callReplyIsResp3` (call_reply.c:561-563)
    pub fn is_resp3(&self) -> bool {
        self.flags & REPLY_FLAG_RESP3 != 0
    }

    /// Return the deferred error list, if any.
    ///
    /// C: `callReplyDeferredErrorList` (call_reply.c:566-568)
    pub fn deferred_error_list(&self) -> Option<&Vec<Vec<u8>>> {
        self.deferred_error_list.as_ref()
    }
}

// Drop replaces freeCallReply / freeCallReplyInternal.
// The recursive structure (Vec<CallReply>, Option<Box<CallReply>>) is
// dropped automatically; Vec<u8> fields are freed by Vec's Drop.
//
// PORT NOTE: In C, `freeCallReply` checks REPLY_FLAG_ROOT to guard against
// double-free on sub-reply pointers.  In Rust, ownership prevents this:
// sub-replies are owned by their parent's Vec, not shared.  The ROOT guard is
// still preserved conceptually via `original_proto` (only present on root).
//
// C: `freeCallReply` (call_reply.c:284-298), `freeCallReplyInternal` (call_reply.c:259-279)
impl Drop for CallReply {
    fn drop(&mut self) {
        // All owned fields (original_proto, proto, val, attribute,
        // deferred_error_list) are dropped automatically by Rust.
        // Nothing manual required.
    }
}

// ── CallReplyParserCtx ────────────────────────────────────────────────────
// Implements ParserCallbacks to build a CallReply node tree from a RESP buffer.
// Each instance wraps a mutable reference to the CallReply node currently
// being filled.
//
// C: `CallReplyParserCallbacks` (call_reply.c:317-334) — callback table wired
// via void *ctx pointing to a CallReply node.

/// Parser context for building a `CallReply` tree.
///
/// Unlike the C design (which passed `CallReply *` directly as `void *ctx`),
/// `CallReplyParserCtx` holds the node being populated by value during parsing
/// of child nodes, then moves the result back into the parent's `Vec`.
///
/// The root-level parse uses a different approach: `ensure_parsed` passes a
/// mutable reference to the `CallReply` directly.
struct CallReplyParserCtx<'a> {
    reply: &'a mut CallReply,
    exact_type: bool,
    private_data: Option<Arc<dyn Any + Send + Sync>>,
}

impl<'a> CallReplyParserCtx<'a> {
    fn new(reply: &'a mut CallReply, exact_type: bool) -> Self {
        let private_data = reply.private_data.clone();
        Self { reply, exact_type, private_data }
    }

    /// Parse `count` child elements from `cursor` and return them as a `Vec<CallReply>`.
    ///
    /// Propagates `REPLY_FLAG_RESP3` from any child that has it up to the parent flags.
    ///
    /// C: inner loop of `callReplyParseCollection` (call_reply.c:198-208)
    fn parse_children(
        cursor: &mut ParserCursor<'_>,
        count: usize,
        private_data: Option<Arc<dyn Any + Send + Sync>>,
        exact_type: bool,
        parent_flags: &mut u32,
    ) -> Vec<CallReply> {
        let mut children = Vec::with_capacity(count);
        for _ in 0..count {
            let mut child = CallReply::new_child(private_data.clone());
            if exact_type {
                child.flags |= REPLY_FLAG_EXACT_TYPE;
            }
            {
                let mut child_ctx = CallReplyParserCtx::new(&mut child, exact_type);
                let _ = cursor.parse_next(&mut child_ctx);
            }
            child.flags |= REPLY_FLAG_PARSED;
            if child.flags & REPLY_FLAG_RESP3 != 0 {
                *parent_flags |= REPLY_FLAG_RESP3;
            }
            children.push(child);
        }
        children
    }
}

/// C: `callReplyParserCallbacks` — each callback sets fields on the target node.
impl<'a> ParserCallbacks for CallReplyParserCtx<'a> {
    /// C: `callReplyNull` — RESP3 null `_\r\n`
    fn on_null(&mut self, proto: &[u8]) {
        self.reply.reply_type = CallReplyType::Null;
        self.reply.proto = proto.to_vec();
        self.reply.flags |= REPLY_FLAG_RESP3;
    }

    /// C: `callReplyNullBulkString` — RESP2 `$-1\r\n`
    fn on_null_bulk_string(&mut self, proto: &[u8]) {
        self.reply.reply_type = CallReplyType::Null;
        self.reply.proto = proto.to_vec();
    }

    /// C: `callReplyNullArray` — RESP2 `*-1\r\n`
    ///
    /// With `REPLY_FLAG_EXACT_TYPE`: type is `ArrayNull`.
    /// Without: type is `Null`.
    fn on_null_array(&mut self, proto: &[u8]) {
        self.reply.reply_type = if self.exact_type {
            CallReplyType::ArrayNull
        } else {
            CallReplyType::Null
        };
        self.reply.proto = proto.to_vec();
    }

    /// C: `callReplyBulkString` — RESP2/3 bulk string `$<n>\r\n<data>\r\n`
    fn on_bulk_string(&mut self, data: &[u8], proto: &[u8]) {
        self.reply.reply_type = CallReplyType::String;
        self.reply.proto = proto.to_vec();
        self.reply.len = data.len();
        self.reply.val = CallReplyValue::Str(data.to_vec());
    }

    /// C: `callReplyError` — RESP error `-<msg>\r\n`
    fn on_error(&mut self, data: &[u8], proto: &[u8]) {
        self.reply.reply_type = CallReplyType::Error;
        self.reply.proto = proto.to_vec();
        self.reply.len = data.len();
        self.reply.val = CallReplyValue::Str(data.to_vec());
    }

    /// C: `callReplySimpleStr` — RESP2 `+<msg>\r\n`
    ///
    /// With `REPLY_FLAG_EXACT_TYPE`: preserves `SimpleString`.
    /// Without: returns as generic `String`.
    fn on_simple_str(&mut self, data: &[u8], proto: &[u8]) {
        self.reply.reply_type = if self.exact_type {
            CallReplyType::SimpleString
        } else {
            CallReplyType::String
        };
        self.reply.proto = proto.to_vec();
        self.reply.len = data.len();
        self.reply.val = CallReplyValue::Str(data.to_vec());
    }

    /// C: `callReplyLong` — RESP integer `:<n>\r\n`
    fn on_long(&mut self, val: i64, proto: &[u8]) {
        self.reply.reply_type = CallReplyType::Integer;
        self.reply.proto = proto.to_vec();
        self.reply.val = CallReplyValue::LongLong(val);
    }

    /// C: `callReplyDouble` — RESP3 double `,<f>\r\n`
    fn on_double(&mut self, val: f64, proto: &[u8]) {
        self.reply.reply_type = CallReplyType::Double;
        self.reply.proto = proto.to_vec();
        self.reply.flags |= REPLY_FLAG_RESP3;
        self.reply.val = CallReplyValue::Double(val);
    }

    /// C: `callReplyBool` — RESP3 boolean `#t\r\n` / `#f\r\n`
    fn on_bool(&mut self, val: bool, proto: &[u8]) {
        self.reply.reply_type = CallReplyType::Bool;
        self.reply.proto = proto.to_vec();
        self.reply.flags |= REPLY_FLAG_RESP3;
        self.reply.val = CallReplyValue::LongLong(val as i64);
    }

    /// C: `callReplyBigNumber` — RESP3 big number `(<digits>\r\n`
    fn on_big_number(&mut self, data: &[u8], proto: &[u8]) {
        self.reply.reply_type = CallReplyType::BigNumber;
        self.reply.proto = proto.to_vec();
        self.reply.flags |= REPLY_FLAG_RESP3;
        self.reply.len = data.len();
        self.reply.val = CallReplyValue::Str(data.to_vec());
    }

    /// C: `callReplyVerbatimString` — RESP3 verbatim `=<n>\r\n<fmt>:<data>\r\n`
    fn on_verbatim_string(&mut self, format: &[u8], data: &[u8], proto: &[u8]) {
        self.reply.reply_type = CallReplyType::VerbatimString;
        self.reply.proto = proto.to_vec();
        self.reply.flags |= REPLY_FLAG_RESP3;
        self.reply.len = data.len();
        self.reply.val = CallReplyValue::VerbatimStr {
            data: data.to_vec(),
            format: format.to_vec(),
        };
    }

    /// C: `callReplyArray` — RESP array `*<n>\r\n` elements…
    fn on_array(&mut self, cursor: &mut ParserCursor<'_>, len: i64, proto: &[u8]) {
        let count = len.max(0) as usize;
        self.reply.reply_type = CallReplyType::Array;
        self.reply.len = count;
        let children = Self::parse_children(
            cursor,
            count,
            self.private_data.clone(),
            self.exact_type,
            &mut self.reply.flags,
        );
        self.reply.proto = proto.to_vec();
        self.reply.val = CallReplyValue::Array(children);
    }

    /// C: `callReplySet` — RESP3 set `~<n>\r\n` elements…
    fn on_set(&mut self, cursor: &mut ParserCursor<'_>, len: i64, proto: &[u8]) {
        let count = len.max(0) as usize;
        self.reply.reply_type = CallReplyType::Set;
        self.reply.len = count;
        let children = Self::parse_children(
            cursor,
            count,
            self.private_data.clone(),
            self.exact_type,
            &mut self.reply.flags,
        );
        self.reply.proto = proto.to_vec();
        self.reply.flags |= REPLY_FLAG_RESP3;
        self.reply.val = CallReplyValue::Array(children);
    }

    /// C: `callReplyMap` — RESP3 map `%<n>\r\n` key value … (2*n elements)
    fn on_map(&mut self, cursor: &mut ParserCursor<'_>, len: i64, proto: &[u8]) {
        let count = len.max(0) as usize;
        self.reply.reply_type = CallReplyType::Map;
        self.reply.len = count;
        let children = Self::parse_children(
            cursor,
            count * 2,
            self.private_data.clone(),
            self.exact_type,
            &mut self.reply.flags,
        );
        self.reply.proto = proto.to_vec();
        self.reply.flags |= REPLY_FLAG_RESP3;
        self.reply.val = CallReplyValue::Array(children);
    }

    /// C: `callReplyAttribute` — RESP3 attribute `|<n>\r\n` key value … element
    ///
    /// Parses the attribute map (2*n key-value pairs) into a separate
    /// `CallReply` node stored in `self.reply.attribute`, then parses the actual
    /// reply element that follows and merges it into `self.reply`.
    ///
    /// C: `callReplyAttribute` (call_reply.c:213-231)
    fn on_attribute(&mut self, cursor: &mut ParserCursor<'_>, len: i64, proto: &[u8]) {
        let count = len.max(0) as usize;

        // Build the attribute sub-reply (a MAP-like node).
        let mut attr = CallReply::new_child(self.private_data.clone());
        attr.reply_type = CallReplyType::Attribute;
        attr.len = count;
        let attr_children = Self::parse_children(
            cursor,
            count * 2,
            self.private_data.clone(),
            self.exact_type,
            &mut attr.flags,
        );
        attr.flags |= REPLY_FLAG_PARSED | REPLY_FLAG_RESP3;
        attr.proto = proto.to_vec();
        attr.val = CallReplyValue::Array(attr_children);

        // Parse the actual reply element that follows the attribute section.
        // In C: `parseReply(parser, rep)` — re-enters on the same node.
        // In Rust: parse into a temporary node, then merge fields.
        let mut actual = CallReply::new_child(self.private_data.clone());
        if self.exact_type {
            actual.flags |= REPLY_FLAG_EXACT_TYPE;
        }
        {
            let mut actual_ctx = CallReplyParserCtx::new(&mut actual, self.exact_type);
            let _ = cursor.parse_next(&mut actual_ctx);
        }
        actual.flags |= REPLY_FLAG_PARSED;

        // Merge the actual reply's fields into self.reply.
        // PORT NOTE: In C, proto covers the full span from attribute start to
        // end of the actual reply.  Here both sub-proto slices are copies; the
        // full merged proto is not reconstructed.  Phase B should concatenate.
        self.reply.reply_type = actual.reply_type;
        self.reply.len = actual.len;
        self.reply.val = std::mem::take(&mut actual.val);
        self.reply.flags |= actual.flags | REPLY_FLAG_RESP3;
        self.reply.proto = proto.to_vec();
        self.reply.attribute = Some(Box::new(attr));
    }

    /// C: `callReplyParseError` — unknown byte in RESP stream
    fn on_parse_error(&mut self) {
        self.reply.reply_type = CallReplyType::Unknown;
    }
}

// ── RespHandlersParserCtx ─────────────────────────────────────────────────
// Implements ParserCallbacks to dispatch a live RESP reply to the module's
// ValkeyModuleReplyHandlers callbacks.
//
// C: `ReplyHandlersParserCallbacks` (call_reply.c:784-801) via `RespHandlersCtx`
// (call_reply.c:634-637).

/// Internal adapter pairing a `ValkeyModuleReplyHandlers` table with its
/// associated module context.
///
/// C: `RespHandlersCtx` (call_reply.c:634-637)
struct RespHandlersParserCtx<'a> {
    handlers: &'a ValkeyModuleReplyHandlers,
}

impl<'a> RespHandlersParserCtx<'a> {
    fn new(handlers: &'a ValkeyModuleReplyHandlers) -> Self {
        Self { handlers }
    }
}

impl<'a> ParserCallbacks for RespHandlersParserCtx<'a> {
    /// C: `replyHandlersNull`
    fn on_null(&mut self, _proto: &[u8]) {
        if let Some(f) = self.handlers.null {
            f();
        }
    }

    /// C: `replyHandlersNullBulkString`
    fn on_null_bulk_string(&mut self, _proto: &[u8]) {
        if let Some(f) = self.handlers.null_bulk_string {
            f();
        }
    }

    /// C: `replyHandlersNullArray`
    fn on_null_array(&mut self, _proto: &[u8]) {
        if let Some(f) = self.handlers.null_array {
            f();
        }
    }

    /// C: `replyHandlersBulkString`
    fn on_bulk_string(&mut self, data: &[u8], _proto: &[u8]) {
        if let Some(f) = self.handlers.bulk_string {
            f(data);
        }
    }

    /// C: `replyHandlersError`
    fn on_error(&mut self, _data: &[u8], _proto: &[u8]) {
        if let Some(f) = self.handlers.reply_parsing_error {
            // TODO(port): C error callback receives (str, len); module handler
            // `reply_parsing_error` takes no args.  There is no direct match in
            // ValkeyModuleReplyHandlers for error payloads with data.  Check
            // valkeymodule.h:1376-1411 in Phase 10 for the correct mapping.
            f();
        }
    }

    /// C: `replyHandlersSimpleStr`
    fn on_simple_str(&mut self, data: &[u8], _proto: &[u8]) {
        if let Some(f) = self.handlers.simple_string {
            f(data);
        }
    }

    /// C: `replyHandlersLong`
    fn on_long(&mut self, val: i64, _proto: &[u8]) {
        if let Some(f) = self.handlers.integer {
            f(val);
        }
    }

    /// C: `replyHandlersDouble`
    fn on_double(&mut self, val: f64, _proto: &[u8]) {
        if let Some(f) = self.handlers.double_val {
            f(val);
        }
    }

    /// C: `replyHandlersBool`
    fn on_bool(&mut self, val: bool, _proto: &[u8]) {
        if let Some(f) = self.handlers.bool_val {
            f(val);
        }
    }

    /// C: `replyHandlersBigNumber`
    fn on_big_number(&mut self, data: &[u8], _proto: &[u8]) {
        if let Some(f) = self.handlers.big_number {
            f(data);
        }
    }

    /// C: `replyHandlersVerbatimString`
    fn on_verbatim_string(&mut self, format: &[u8], data: &[u8], _proto: &[u8]) {
        if let Some(f) = self.handlers.verbatim_string {
            f(data, format);
        }
    }

    /// C: `replyHandlersArray` — always consumes `len` children even if callback is None.
    fn on_array(&mut self, cursor: &mut ParserCursor<'_>, len: i64, _proto: &[u8]) {
        let count = len.max(0) as usize;
        if let Some(f) = self.handlers.array_start {
            f(count);
        }
        for _ in 0..count {
            let _ = cursor.parse_next(self);
        }
        if let Some(f) = self.handlers.array_end {
            f();
        }
    }

    /// C: `replyHandlersSet`
    fn on_set(&mut self, cursor: &mut ParserCursor<'_>, len: i64, _proto: &[u8]) {
        let count = len.max(0) as usize;
        if let Some(f) = self.handlers.set_start {
            f(count);
        }
        for _ in 0..count {
            let _ = cursor.parse_next(self);
        }
        if let Some(f) = self.handlers.set_end {
            f();
        }
    }

    /// C: `replyHandlersMap`
    fn on_map(&mut self, cursor: &mut ParserCursor<'_>, len: i64, _proto: &[u8]) {
        let count = len.max(0) as usize;
        if let Some(f) = self.handlers.map_start {
            f(count);
        }
        for _ in 0..(count * 2) {
            let _ = cursor.parse_next(self);
        }
        if let Some(f) = self.handlers.map_end {
            f();
        }
    }

    /// C: `replyHandlersAttribute`
    ///
    /// Parses 2*len attribute key-value pairs, then parses the element that
    /// follows the attribute section.
    fn on_attribute(&mut self, cursor: &mut ParserCursor<'_>, len: i64, _proto: &[u8]) {
        let count = len.max(0) as usize;
        if let Some(f) = self.handlers.attribute_start {
            f(count);
        }
        for _ in 0..(count * 2) {
            let _ = cursor.parse_next(self);
        }
        if let Some(f) = self.handlers.attribute_end {
            f();
        }
        // C: `parseReply(parser, ctx)` — parse the element following the attribute.
        let _ = cursor.parse_next(self);
    }

    /// C: `replyHandlersParseError`
    fn on_parse_error(&mut self) {
        if let Some(f) = self.handlers.reply_parsing_error {
            f();
        }
    }
}

// ── Public function: invoke_reply_handlers ────────────────────────────────
// C: `invokeReplyHandlers` (call_reply.c:813-868)
//
// Gathers the client's output buffer, optionally delivers it to
// `handlers.on_resp_available`, then walks the RESP reply frame-by-frame
// dispatching to the per-type handler callbacks.

/// Parse the RESP reply accumulated in `reply_bytes` and deliver it to `handlers`.
///
/// If `handlers.on_resp_available` is set it is called first with the raw bytes.
/// When it returns `false`, per-type callbacks are skipped; when it returns `true`
/// (or the field is `None`), the reply is walked recursively.
///
/// C: `invokeReplyHandlers` (call_reply.c:813-868)
///
/// PORT NOTE: The C function receives a `client *c` and drains its static buffer
/// plus linked-list reply segments.  The Rust version receives the already-
/// assembled `reply_bytes` slice, with buffer draining handled by the caller
/// (networking layer).  Phase 3 will integrate this with the real `Client` reply
/// buffer once the networking architecture is settled.
///
/// TODO(architect): Integrate with `Client::drain_reply` or equivalent once
/// the networking / event-loop split is decided in Phase 3.
pub fn invoke_reply_handlers(
    ctx: &ValkeyModuleCtx,
    reply_bytes: &[u8],
    handlers: &ValkeyModuleReplyHandlers,
) {
    let mut continue_parsing = true;

    if let Some(on_resp) = handlers.on_resp_available {
        continue_parsing = on_resp(ctx, reply_bytes);
    }

    if continue_parsing {
        let mut dispatch_ctx = RespHandlersParserCtx::new(handlers);
        let mut cursor = ParserCursor::new(reply_bytes);
        let _ = cursor.parse_next(&mut dispatch_ctx);
    }
}

// ──────────────────────────────────────────────────────────────────────────
// PORT STATUS
//   source:        src/call_reply.c  (869 lines, ~46 functions)
//                  src/call_reply.h  (65 lines)
//   target_crate:  redis-core
//   confidence:    high
//   todos:         6
//   port_notes:    10
//   unsafe_blocks: 0
//   notes:         Compiles cleanly within the crate (zero errors, zero warnings
//                  from this file).  Two main architectural departures from C:
//                  (1) string/proto data is owned per-node (Vec<u8> copy) rather
//                  than a zero-copy pointer-into-root-buffer — PERF(port) flagged,
//                  consider bytes::Bytes in Phase B;
//                  (2) private_data is Arc<dyn Any+Send+Sync> instead of void* —
//                  needs architect decision in Phase 10 when module layer lands.
//                  Bug in upstream C code replicated faithfully:
//                  callReplyGetAttributeElement passes MAP type not ATTRIBUTE to
//                  the internal type-check (see PORT NOTE in get_attribute_element).
//                  invoke_reply_handlers signature simplified — buffer draining
//                  moved to caller; Phase 3 networking integration via TODO(architect).
// ──────────────────────────────────────────────────────────────────────────
