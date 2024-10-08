//! Linux utilities

use crate::errors::{IOError, IOResult};

/// Converts a return value from a syscall into a [IOResult] type.
#[inline(always)]
#[allow(unused)]
pub(crate) fn from_ret(value: usize) -> IOResult<usize> {
    if value > -4096isize as usize {
        // Truncation of the error value is guaranteed to never occur due to
        // the above check. This is the same check that musl uses:
        // https://git.musl-libc.org/cgit/musl/tree/src/internal/syscall_ret.c?h=v1.1.15
        Err(IOError(-(value as i32)))
    } else {
        Ok(value)
    }
}
