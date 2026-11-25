// SPDX-License-Identifier: Apache-2.0

//! C functions that tests reach but Miri doesn't implement.
//!
//! Miri resolves a foreign call to a `#[no_mangle]` Rust function of that name, so defining them
//! here lets Miri keep checking the code past the call. Only built under Miri.

use std::collections::hash_map::RandomState;
use std::ffi::{CStr, OsStr};
use std::hash::{BuildHasher, Hasher};
use std::os::unix::ffi::OsStrExt;

use libc::{c_char, c_int, off64_t, size_t, ssize_t};

#[no_mangle]
unsafe extern "C" fn mkdtemp(template: *mut c_char) -> *mut c_char {
    let len = CStr::from_ptr(template).to_bytes().len();
    let bytes = std::slice::from_raw_parts_mut(template as *mut u8, len);
    if !bytes.ends_with(b"XXXXXX") {
        return std::ptr::null_mut();
    }
    for _ in 0..64 {
        let mut n = RandomState::new().build_hasher().finish();
        for b in &mut bytes[len - 6..] {
            *b = b'a' + (n % 26) as u8;
            n /= 26;
        }
        if std::fs::create_dir(OsStr::from_bytes(bytes)).is_ok() {
            return template;
        }
    }
    std::ptr::null_mut()
}

/// Only a hint to the kernel, so doing nothing is a valid implementation.
#[no_mangle]
extern "C" fn readahead(_fd: c_int, _offset: off64_t, _count: size_t) -> ssize_t {
    0
}

#[no_mangle]
extern "C" fn getegid() -> libc::gid_t {
    0
}

/// Report that the file has no extended attributes.
#[no_mangle]
extern "C" fn llistxattr(_path: *const c_char, _list: *mut c_char, _size: size_t) -> ssize_t {
    0
}
