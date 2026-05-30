//! Native C FFI for BSX v2 — Windows / Linux / Android
//!
//! Exposes the BSX decode API as a plain C interface for use from
//! any language with FFI support (C, C++, GDNative, JNI, etc.).
//!
//! ## Lifecycle
//!
//!   BsxHandle h = bsx_load(data, len);   // NULL on error
//!   // ... use h ...
//!   bsx_free(h);
//!
//! ## Getting asset bytes
//!
//!   size_t sz;
//!   uint8_t* buf = bsx_get(h, "sprites/player.png", &sz);
//!   // ... use buf[0..sz] ...
//!   bsx_free_buf(buf, sz);
//!
//! ## Listing assets
//!
//!   char** list = bsx_list(h);            // NULL-terminated array
//!   for (char** p = list; *p; ++p) { ... }
//!   bsx_free_list(list);
//!
//! ## String pool
//!
//!   int32_t count = bsx_key(h, "password");
//!   size_t slen;
//!   uint8_t* s = bsx_string(h, 0, &slen); // UTF-8, not null-terminated
//!   bsx_free_buf(s, slen);
//!
//! ## Thread safety
//!
//! A single `BsxHandle` is NOT thread-safe for concurrent mutation
//! (`bsx_key` mutates internal state). Reads (`bsx_get`, `bsx_has`,
//! `bsx_list`) may be called from multiple threads on the same handle
//! as long as `bsx_key` has already returned.

#![cfg(feature = "ffi")]

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};

use crate::BsxBundle;

// ─── opaque handle ────────────────────────────────────────────────────────────

/// Opaque handle to a loaded BSX bundle.
/// NULL == invalid / error.
pub type BsxHandle = *mut BsxBundle;

// ─── helpers ──────────────────────────────────────────────────────────────────

#[inline]
unsafe fn bref<'a>(h: BsxHandle) -> Option<&'a BsxBundle> {
    if h.is_null() { None } else { Some(&*h) }
}

#[inline]
unsafe fn bmut<'a>(h: BsxHandle) -> Option<&'a mut BsxBundle> {
    if h.is_null() { None } else { Some(&mut *h) }
}

/// `*const c_char` → `&str`, returns None on null or invalid UTF-8.
#[inline]
unsafe fn cstr_to_str(ptr: *const c_char) -> Option<&'static str> {
    if ptr.is_null() { return None; }
    CStr::from_ptr(ptr).to_str().ok()
}

/// Move a `Vec<u8>` onto the heap and hand out a raw pointer + length.
/// Caller must free with `bsx_free_buf(ptr, len)`.
unsafe fn vec_to_raw(mut v: Vec<u8>, out_len: *mut usize) -> *mut u8 {
    v.shrink_to_fit();
    *out_len = v.len();
    let ptr = v.as_mut_ptr();
    std::mem::forget(v);
    ptr
}

// ─── lifecycle ────────────────────────────────────────────────────────────────

/// Load a BSX bundle from a byte buffer.
///
/// `data` is copied internally — the caller may free it after this call.
/// Returns NULL on parse error.
/// Free with [`bsx_free`].
#[no_mangle]
pub unsafe extern "C" fn bsx_load(data: *const u8, len: usize) -> BsxHandle {
    if data.is_null() || len == 0 { return std::ptr::null_mut(); }
    let bytes = std::slice::from_raw_parts(data, len).to_vec();
    match BsxBundle::from_bytes(bytes) {
        Ok(b)  => Box::into_raw(Box::new(b)),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a handle returned by [`bsx_load`].
/// Passing NULL is a no-op.
#[no_mangle]
pub unsafe extern "C" fn bsx_free(handle: BsxHandle) {
    if !handle.is_null() {
        drop(Box::from_raw(handle));
    }
}

// ─── query ────────────────────────────────────────────────────────────────────

/// Returns 1 if `path` exists in the bundle, 0 otherwise.
#[no_mangle]
pub unsafe extern "C" fn bsx_has(handle: BsxHandle, path: *const c_char) -> u8 {
    let (Some(b), Some(p)) = (bref(handle), cstr_to_str(path)) else { return 0; };
    b.has(p) as u8
}

/// Asset kind:
///   0 = IMG   (decoded RGBA8)
///   1 = AUDIO (OGG/WAV bytes)
///   2 = BLOB  (raw bytes)
///
/// Returns 2 (BLOB) on invalid handle or unknown path.
#[no_mangle]
pub unsafe extern "C" fn bsx_asset_type(handle: BsxHandle, path: *const c_char) -> u8 {
    let (Some(b), Some(p)) = (bref(handle), cstr_to_str(path)) else { return 2; };
    match b.asset_type(p) {
        "IMG"   => 0,
        "AUDIO" => 1,
        _       => 2,
    }
}

/// Returns the stored (compressed) byte size of the asset, or 0 if not found.
#[no_mangle]
pub unsafe extern "C" fn bsx_size(handle: BsxHandle, path: *const c_char) -> u32 {
    let (Some(b), Some(p)) = (bref(handle), cstr_to_str(path)) else { return 0; };
    b.size(p) as u32
}

/// For IMG assets, writes pixel dimensions into `*out_w` / `*out_h`.
/// Returns 1 on success, 0 if not an IMG or not found.
/// Either out pointer may be NULL (that dimension is skipped).
#[no_mangle]
pub unsafe extern "C" fn bsx_dimensions(
    handle: BsxHandle,
    path:   *const c_char,
    out_w:  *mut u32,
    out_h:  *mut u32,
) -> u8 {
    let (Some(b), Some(p)) = (bref(handle), cstr_to_str(path)) else { return 0; };
    match b.dimensions(p) {
        Some((w, h)) => {
            if !out_w.is_null() { *out_w = w; }
            if !out_h.is_null() { *out_h = h; }
            1
        }
        None => 0,
    }
}

// ─── get ──────────────────────────────────────────────────────────────────────

/// Decode and return asset bytes.
///
/// On success: allocates a buffer, writes its length to `*out_len`, returns the pointer.
/// Caller **must** free with `bsx_free_buf(ptr, *out_len)`.
///
/// On error: returns NULL, `*out_len` is set to 0.
///
/// For IMG assets the returned bytes are raw RGBA8 (width × height × 4).
/// For AUDIO assets the returned bytes are raw WAV PCM / OGG container.
/// For BLOB assets the returned bytes are verbatim stored data.
#[no_mangle]
pub unsafe extern "C" fn bsx_get(
    handle:  BsxHandle,
    path:    *const c_char,
    out_len: *mut usize,
) -> *mut u8 {
    if out_len.is_null() { return std::ptr::null_mut(); }
    *out_len = 0;
    let (Some(b), Some(p)) = (bref(handle), cstr_to_str(path)) else { return std::ptr::null_mut(); };
    match b.get(p) {
        Ok(v)  => vec_to_raw(v, out_len),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a buffer that was returned by [`bsx_get`] or [`bsx_string`].
///
/// `len` must be the exact value that was written to `out_len` on the
/// originating call. Passing NULL or len=0 is a no-op.
#[no_mangle]
pub unsafe extern "C" fn bsx_free_buf(ptr: *mut u8, len: usize) {
    if !ptr.is_null() && len > 0 {
        drop(Vec::from_raw_parts(ptr, len, len));
    }
}

// ─── list ─────────────────────────────────────────────────────────────────────

/// Return a NULL-terminated array of C strings listing every asset path
/// in the bundle (assets + audio tracks).
///
/// Free with [`bsx_free_list`].
/// Returns NULL on invalid handle.
#[no_mangle]
pub unsafe extern "C" fn bsx_list(handle: BsxHandle) -> *mut *mut c_char {
    let Some(b) = bref(handle) else { return std::ptr::null_mut(); };
    let paths = b.list_all();
    // Convert each path to an owned CString, then leak it.
    let mut ptrs: Vec<*mut c_char> = paths
        .into_iter()
        .filter_map(|s| CString::new(s).ok())
        .map(|cs| cs.into_raw())
        .collect();
    ptrs.push(std::ptr::null_mut()); // NULL sentinel
    ptrs.shrink_to_fit();
    let ptr = ptrs.as_mut_ptr();
    std::mem::forget(ptrs);
    ptr
}

/// Free a list returned by [`bsx_list`].
/// Passing NULL is a no-op.
#[no_mangle]
pub unsafe extern "C" fn bsx_free_list(list: *mut *mut c_char) {
    if list.is_null() { return; }
    // Count entries and reclaim each CString.
    let mut n = 0usize;
    loop {
        let p = *list.add(n);
        if p.is_null() { break; }
        drop(CString::from_raw(p));
        n += 1;
    }
    // Reclaim the pointer array itself (len == cap after shrink_to_fit).
    drop(Vec::from_raw_parts(list, n + 1, n + 1));
}

// ─── string pool ──────────────────────────────────────────────────────────────

/// Unlock the string pool embedded in the bundle.
///
/// Returns the number of strings (≥ 0) on success, or -1 on error
/// (wrong password, no pool, or invalid handle).
///
/// Must be called before [`bsx_string`].
#[no_mangle]
pub unsafe extern "C" fn bsx_key(handle: BsxHandle, password: *const c_char) -> c_int {
    let (Some(b), Some(p)) = (bmut(handle), cstr_to_str(password)) else { return -1; };
    match b.key(p) {
        Err(_) => -1,
        Ok(()) => b.strings_count().map(|n| n as c_int).unwrap_or(-1),
    }
}

/// Retrieve the string at `index` from the unlocked pool.
///
/// Returns a heap-allocated UTF-8 byte buffer (NOT null-terminated).
/// `*out_len` is set to the byte length.
/// Caller must free with `bsx_free_buf(ptr, *out_len)`.
///
/// Returns NULL if the pool is not unlocked, or `index` is out of bounds.
#[no_mangle]
pub unsafe extern "C" fn bsx_string(
    handle:  BsxHandle,
    index:   u32,
    out_len: *mut usize,
) -> *mut u8 {
    if out_len.is_null() { return std::ptr::null_mut(); }
    *out_len = 0;
    let Some(b) = bref(handle) else { return std::ptr::null_mut(); };
    match b.string(index as usize) {
        Err(_) => std::ptr::null_mut(),
        Ok(s)  => vec_to_raw(s.as_bytes().to_vec(), out_len),
    }
}
