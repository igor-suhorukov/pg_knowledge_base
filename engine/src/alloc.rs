// SPDX-License-Identifier: Apache-2.0
//! The engine's memory, counted (docs/design.md §2.2): a backend's memory
//! contexts do not see what Rust allocates -- hash tables of joins, sorts,
//! groups -- so the library counts it, and a call whose allocations pass
//! kb.engine_memory_limit fails at its next callback, row or quad, before
//! the server's memory does.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::error::{EResult, EngineError};

static USED: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(0);
/// The most USED reached since the last reset (peak()), for traces
static PEAK: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn grew(now: usize) {
    if now > PEAK.load(Ordering::Relaxed) {
        PEAK.store(now, Ordering::Relaxed);
    }
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(USED.fetch_add(layout.size(), Ordering::Relaxed) + layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(USED.fetch_add(layout.size(), Ordering::Relaxed) + layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        USED.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            grew(USED.fetch_add(new_size, Ordering::Relaxed) + new_size);
            USED.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

pub fn set_limit(bytes: usize) {
    LIMIT.store(bytes, Ordering::Relaxed);
}

pub fn used() -> usize {
    USED.load(Ordering::Relaxed)
}

/// The most the engine held since the last reset; with reset, starts again
/// from what it holds now.
pub fn peak(reset: bool) -> usize {
    let peak = PEAK.load(Ordering::Relaxed).max(used());
    if reset {
        PEAK.store(used(), Ordering::Relaxed);
    }
    peak
}

/// An error once the engine holds more than its limit.
pub fn check() -> EResult<()> {
    let limit = LIMIT.load(Ordering::Relaxed);
    if limit > 0 && used() > limit {
        return Err(EngineError::new(
            crate::error::KB_ERR_MEMORY,
            format!(
                "the query holds more of the engine's memory than kb.engine_memory_limit allows ({})",
                setting_text(limit)
            ),
        ));
    }
    Ok(())
}

/// A size as PostgreSQL shows a setting of kilobytes
fn setting_text(bytes: usize) -> String {
    let kb = bytes >> 10;
    if kb > 0 && kb % (1 << 20) == 0 {
        format!("{}GB", kb >> 20)
    } else if kb > 0 && kb % (1 << 10) == 0 {
        format!("{}MB", kb >> 10)
    } else {
        format!("{kb}kB")
    }
}
