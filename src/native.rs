//! Types `dumps` serializes natively besides the JSON builtins: `datetime`,
//! `date`, `time`, `uuid.UUID`, dataclasses and `enum.Enum` (issue #5), with
//! orjson's output format.
//!
//! The types are looked up lazily in `sys.modules`, the first time an object
//! that is not a builtin reaches the serializer: an instance can only exist if
//! its module is imported, so nothing is imported (and no Python code runs)
//! from inside `dumps`, and `import rjson` stays cheap. A found type is kept
//! (strong reference) for the life of the process; the module is GIL-only and
//! single-interpreter. A module that is not imported yet is looked up again on
//! the next call.
//!
//! This file only resolves types and formats values; the serializer's
//! handling (including which paths may run Python code) is in `ser.rs`.

use pyo3::ffi;
use std::ffi::CStr;
use std::ptr;

/// `passthrough=` flags: these kinds go to `default=` (or raise) instead of
/// being serialized natively. Values are part of the public API
/// (`rjson.PASSTHROUGH_*`).
pub const PT_DATETIME: u32 = 1;
pub const PT_UUID: u32 = 2;
pub const PT_DATACLASS: u32 = 4;
pub const PT_ENUM: u32 = 8;
pub const PT_ALL: u32 = PT_DATETIME | PT_UUID | PT_DATACLASS | PT_ENUM;

pub struct Types {
    pub datetime: *mut ffi::PyTypeObject,
    pub date: *mut ffi::PyTypeObject,
    pub time: *mut ffi::PyTypeObject,
    /// `datetime.timezone`: its `utcoffset` is C code.
    pub timezone: *mut ffi::PyTypeObject,
    /// `zoneinfo.ZoneInfo` if it is the C implementation (checked once:
    /// `zoneinfo_checked`), else null.
    pub zoneinfo: *mut ffi::PyTypeObject,
    /// `ZoneInfo.utcoffset` (a method descriptor), called directly.
    pub zoneinfo_utcoffset: *mut ffi::PyObject,
    pub timedelta: *mut ffi::PyTypeObject,
    zoneinfo_checked: bool,
    pub uuid: *mut ffi::PyTypeObject,
    /// Reading `UUID.int` runs no Python code (plain attribute lookup ending
    /// in the `__slots__` member descriptor). Checked once when `uuid` is
    /// found; if false, UUIDs are serialized in guarded mode.
    pub uuid_plain: bool,
    /// The `int` slot of `UUID` (`__slots__` member), read with
    /// `PyMember_GetOne`; null if not found.
    pub uuid_int: *mut ffi::PyMemberDef,
    pub enum_meta: *mut ffi::PyTypeObject,
    /// `dataclasses._FIELD`: the field kind of real fields (not ClassVar or
    /// InitVar pseudo-fields).
    pub dc_field: *mut ffi::PyObject,
}

pub struct Names {
    pub dataclass_fields: *mut ffi::PyObject,
    pub slots: *mut ffi::PyObject,
    pub dict: *mut ffi::PyObject,
    pub value: *mut ffi::PyObject,
    pub int: *mut ffi::PyObject,
    pub utcoffset: *mut ffi::PyObject,
    pub field_type: *mut ffi::PyObject,
    pub sixty_four: *mut ffi::PyObject,
}

static mut TYPES: Types = Types {
    datetime: ptr::null_mut(),
    date: ptr::null_mut(),
    time: ptr::null_mut(),
    timezone: ptr::null_mut(),
    zoneinfo: ptr::null_mut(),
    zoneinfo_utcoffset: ptr::null_mut(),
    timedelta: ptr::null_mut(),
    zoneinfo_checked: false,
    uuid: ptr::null_mut(),
    uuid_plain: false,
    uuid_int: ptr::null_mut(),
    enum_meta: ptr::null_mut(),
    dc_field: ptr::null_mut(),
};

static mut NAMES: Names = Names {
    dataclass_fields: ptr::null_mut(),
    slots: ptr::null_mut(),
    dict: ptr::null_mut(),
    value: ptr::null_mut(),
    int: ptr::null_mut(),
    utcoffset: ptr::null_mut(),
    field_type: ptr::null_mut(),
    sixty_four: ptr::null_mut(),
};

/// `object`'s `tp_getattro`, i.e. `PyObject_GenericGetAttr` as stored in type
/// slots. Read from the type instead of taking the symbol's address, which can
/// differ from the slot value (e.g. an import thunk on Windows).
static mut GENERIC_GETATTR: usize = 0;

/// The type looks attributes up with `object`'s generic `__getattribute__`
/// (no Python `__getattribute__`/`__getattr__`).
#[inline]
pub unsafe fn generic_getattr(ty: *mut ffi::PyTypeObject) -> bool {
    let g = *ptr::addr_of!(GENERIC_GETATTR);
    g != 0 && (*ty).tp_getattro.map(|f| f as usize) == Some(g)
}

/// Creates the interned attribute names. Called once at module init.
pub fn init() -> bool {
    unsafe {
        let base = ptr::addr_of_mut!(ffi::PyBaseObject_Type);
        *ptr::addr_of_mut!(GENERIC_GETATTR) = (*base).tp_getattro.map_or(0, |f| f as usize);
        let n = &mut *ptr::addr_of_mut!(NAMES);
        n.dataclass_fields = ffi::PyUnicode_InternFromString(c"__dataclass_fields__".as_ptr());
        n.slots = ffi::PyUnicode_InternFromString(c"__slots__".as_ptr());
        n.dict = ffi::PyUnicode_InternFromString(c"__dict__".as_ptr());
        n.value = ffi::PyUnicode_InternFromString(c"_value_".as_ptr());
        n.int = ffi::PyUnicode_InternFromString(c"int".as_ptr());
        n.utcoffset = ffi::PyUnicode_InternFromString(c"utcoffset".as_ptr());
        n.field_type = ffi::PyUnicode_InternFromString(c"_field_type".as_ptr());
        n.sixty_four = ffi::PyLong_FromLong(64);
        !(n.dataclass_fields.is_null()
            || n.slots.is_null()
            || n.dict.is_null()
            || n.value.is_null()
            || n.int.is_null()
            || n.utcoffset.is_null()
            || n.field_type.is_null()
            || n.sixty_four.is_null())
    }
}

#[inline(always)]
pub fn names() -> &'static Names {
    // SAFETY: written once in `init` (GIL held), read-only afterwards.
    unsafe { &*ptr::addr_of!(NAMES) }
}

/// `sys.modules[name]` (borrowed) or null. Runs no Python code.
unsafe fn loaded_module(name: &CStr) -> *mut ffi::PyObject {
    let modules = ffi::PyImport_GetModuleDict();
    if modules.is_null() {
        return ptr::null_mut();
    }
    // Suppresses errors; str keys, so no __eq__ runs.
    ffi::PyDict_GetItemString(modules, name.as_ptr())
}

/// `module.name` as a new reference, or null (error cleared). The attribute
/// is an existing module global, so no module `__getattr__` runs.
unsafe fn module_attr(module: *mut ffi::PyObject, name: &CStr) -> *mut ffi::PyObject {
    let o = ffi::PyObject_GetAttrString(module, name.as_ptr());
    if o.is_null() {
        ffi::PyErr_Clear();
    }
    o
}

unsafe fn module_type(module: *mut ffi::PyObject, name: &CStr) -> *mut ffi::PyTypeObject {
    let o = module_attr(module, name);
    if !o.is_null() && ffi::PyType_Check(o) == 0 {
        ffi::Py_DECREF(o);
        return ptr::null_mut();
    }
    o as *mut ffi::PyTypeObject
}

/// `ty.__dict__[name]` (borrowed; only the type's own dict) or null.
/// Runs no Python code: `tp_dict` of a heap type is a plain dict with str keys.
pub unsafe fn own_attr(ty: *mut ffi::PyTypeObject, name: *mut ffi::PyObject) -> *mut ffi::PyObject {
    if (*ty).tp_flags & ffi::Py_TPFLAGS_HEAPTYPE == 0 {
        return ptr::null_mut(); // static types: tp_dict may be per-interpreter
    }
    let d = (*ty).tp_dict;
    if d.is_null() {
        return ptr::null_mut();
    }
    let v = ffi::PyDict_GetItemWithError(d, name);
    if v.is_null() {
        ffi::PyErr_Clear();
    }
    v
}

/// `ty.name` looked up on the class (new reference), or null (error cleared).
/// Only for types whose metaclass is `type`, so no Python code runs; used at
/// resolve time to check how a type implements an attribute.
unsafe fn class_attr(ty: *mut ffi::PyTypeObject, name: *mut ffi::PyObject) -> *mut ffi::PyObject {
    if ffi::Py_TYPE(ty as *mut ffi::PyObject) != ptr::addr_of_mut!(ffi::PyType_Type) {
        return ptr::null_mut();
    }
    let v = ffi::PyObject_GetAttr(ty as *mut ffi::PyObject, name);
    if v.is_null() {
        ffi::PyErr_Clear();
    }
    v
}

/// Resolves the types whose modules are imported by now and returns them.
#[cold]
#[inline(never)]
pub unsafe fn types() -> &'static Types {
    let t = &mut *ptr::addr_of_mut!(TYPES);
    if t.datetime.is_null() && !loaded_module(c"datetime").is_null() {
        // Imports nothing: `datetime` is in sys.modules.
        ffi::PyDateTime_IMPORT();
        let api = ffi::PyDateTimeAPI();
        if api.is_null() {
            ffi::PyErr_Clear();
        } else {
            t.date = (*api).DateType;
            t.time = (*api).TimeType;
            t.timezone = ffi::Py_TYPE((*api).TimeZone_UTC);
            t.timedelta = (*api).DeltaType;
            t.datetime = (*api).DateTimeType; // last: marks the group resolved
        }
    }
    if !t.zoneinfo_checked {
        let m = loaded_module(c"zoneinfo");
        if !m.is_null() {
            t.zoneinfo_checked = true;
            let ty = module_type(m, c"ZoneInfo");
            if !ty.is_null() {
                // The C implementation (`_zoneinfo`) defines utcoffset as a
                // method descriptor; the pure-Python fallback does not.
                let f = class_attr(ty, names().utcoffset);
                let c_impl = !f.is_null()
                    && ffi::Py_TYPE(f) == ptr::addr_of_mut!(ffi::PyMethodDescr_Type)
                    && generic_getattr(ty);
                if c_impl {
                    t.zoneinfo = ty;
                    t.zoneinfo_utcoffset = f; // keeps the reference
                } else {
                    ffi::Py_DECREF(ty as *mut ffi::PyObject);
                    if !f.is_null() {
                        ffi::Py_DECREF(f);
                    }
                }
            }
        }
    }
    if t.uuid.is_null() {
        let m = loaded_module(c"uuid");
        if !m.is_null() {
            let ty = module_type(m, c"UUID");
            if !ty.is_null() {
                let f = class_attr(ty, names().int);
                t.uuid_plain = !f.is_null()
                    && ffi::Py_TYPE(f) == ptr::addr_of_mut!(ffi::PyMemberDescr_Type)
                    && generic_getattr(ty);
                if t.uuid_plain {
                    // `PyMemberDescrObject.d_member` (cpython/descrobject.h;
                    // a `PyMemberDef *` on every supported version, whatever
                    // pyo3-ffi's type for it). Checked by the member's name.
                    let def: *mut ffi::PyMemberDef =
                        (*(f as *mut ffi::PyMemberDescrObject)).d_member.cast();
                    if !def.is_null()
                        && !(*def).name.is_null()
                        && CStr::from_ptr((*def).name) == c"int"
                    {
                        t.uuid_int = def; // valid while the type lives (kept)
                    }
                }
                if !f.is_null() {
                    ffi::Py_DECREF(f);
                }
                t.uuid = ty;
            }
        }
    }
    if t.enum_meta.is_null() {
        let m = loaded_module(c"enum");
        if !m.is_null() {
            t.enum_meta = module_type(m, c"EnumMeta");
        }
    }
    if t.dc_field.is_null() {
        let m = loaded_module(c"dataclasses");
        if !m.is_null() {
            t.dc_field = module_attr(m, c"_FIELD");
        }
    }
    t
}

// ---------------------------------------------------------------------------
// Formatting (orjson's output: RFC 3339, microseconds only when non-zero)
// ---------------------------------------------------------------------------

#[inline(always)]
fn two(b: &mut [u8], at: usize, v: u32) {
    b[at] = b'0' + (v / 10) as u8;
    b[at + 1] = b'0' + (v % 10) as u8;
}

/// `YYYY-MM-DD` into `b[at..at + 10]`.
pub fn fmt_date(b: &mut [u8], at: usize, y: u32, m: u32, d: u32) -> usize {
    two(b, at, y / 100);
    two(b, at + 2, y % 100);
    b[at + 4] = b'-';
    two(b, at + 5, m);
    b[at + 7] = b'-';
    two(b, at + 8, d);
    at + 10
}

/// `HH:MM:SS` or `HH:MM:SS.ffffff` at `b[at..]`; returns the end.
pub fn fmt_time(b: &mut [u8], at: usize, h: u32, mi: u32, s: u32, us: u32) -> usize {
    two(b, at, h);
    b[at + 2] = b':';
    two(b, at + 3, mi);
    b[at + 5] = b':';
    two(b, at + 6, s);
    if us == 0 {
        return at + 8;
    }
    b[at + 8] = b'.';
    two(b, at + 9, us / 10000);
    two(b, at + 11, us / 100 % 100);
    two(b, at + 13, us % 100);
    at + 15
}

/// `+HH:MM` / `-HH:MM` for a UTC offset of `secs` seconds (|secs| < 86400).
///
/// Like orjson, offsets with a seconds part (historical local mean time,
/// e.g. Europe/Amsterdam before 1937: +00:19:32) are rounded to the nearest
/// minute, 30 s rounding up, and keep their sign when they round to zero
/// (`-00:00`). Unlike orjson, a minute that rounds up to 60 carries into the
/// hour (orjson writes `+00:60`, which is not valid RFC 3339).
pub fn fmt_offset(b: &mut [u8], at: usize, secs: i64) -> usize {
    b[at] = if secs < 0 { b'-' } else { b'+' };
    let a = secs.unsigned_abs();
    let mut h = a / 3600;
    let mut m = (a % 3600 + 30) / 60;
    if m == 60 {
        h += 1;
        m = 0;
    }
    if h > 23 {
        // Only for |offset| >= 23:59:30; RFC 3339 has no hour 24.
        h = 23;
        m = 59;
    }
    two(b, at + 1, h as u32);
    b[at + 3] = b':';
    two(b, at + 4, m as u32);
    at + 6
}

/// Lowercase hex pairs for every byte value.
static HEX_PAIRS: [[u8; 2]; 256] = {
    let hex = b"0123456789abcdef";
    let mut t = [[0u8; 2]; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = [hex[i >> 4], hex[i & 15]];
        i += 1;
    }
    t
};

/// Canonical lowercase `8-4-4-4-12` UUID text for the 128-bit value.
pub fn fmt_uuid(b: &mut [u8; 36], v: u128) {
    let bytes = v.to_be_bytes();
    let mut o = 0;
    for (i, byte) in bytes.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            b[o] = b'-';
            o += 1;
        }
        let pair = HEX_PAIRS[*byte as usize];
        b[o] = pair[0];
        b[o + 1] = pair[1];
        o += 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off(secs: i64) -> String {
        let mut b = [0u8; 8];
        let n = fmt_offset(&mut b, 0, secs);
        String::from_utf8(b[..n].to_vec()).unwrap()
    }

    #[test]
    fn offsets() {
        assert_eq!(off(0), "+00:00");
        assert_eq!(off(-19800), "-05:30");
        assert_eq!(off(29), "+00:00");
        assert_eq!(off(30), "+00:01");
        assert_eq!(off(-29), "-00:00");
        assert_eq!(off(1172), "+00:20");
        assert_eq!(off(3599), "+01:00");
        assert_eq!(off(86399), "+23:59");
    }

    #[test]
    fn uuid() {
        let mut b = [0u8; 36];
        fmt_uuid(&mut b, 0x0123456789abcdef0123456789abcdef);
        assert_eq!(&b, b"01234567-89ab-cdef-0123-456789abcdef");
    }
}
