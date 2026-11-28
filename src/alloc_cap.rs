//! Global heap memory tracker and hard ceiling wrapper around `std::alloc::System`.
//!
//! Note: Returning `null_mut()` from `GlobalAlloc` triggers Rust's OOM handler (`abort`),
//! so the hard ceiling acts as a physical last-resort firewall for host safety, while
//! [`is_near_soft_limit`] (70% watermark) lets streaming buffers proactively shed memory.

use std::alloc::{GlobalAlloc, Layout, System};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

const DEFAULT_LIMIT_MB: usize = 16;
const MIN_LIMIT_MB: usize = 4;
const MAX_LIMIT_MB: usize = 256;

static CURRENT_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
static LIMIT_BYTES: AtomicUsize = AtomicUsize::new(DEFAULT_LIMIT_MB * 1024 * 1024);

pub struct CappedAllocator;

#[global_allocator]
pub static GLOBAL_ALLOC: CappedAllocator = CappedAllocator;

/// Configure the hard heap ceiling from `EXT_MODELS_MAX_HEAP_MB` (clamped to 4..=256 MiB).
pub fn init_from_env() {
    if let Ok(raw) = std::env::var("EXT_MODELS_MAX_HEAP_MB") {
        if let Ok(mb) = raw.trim().parse::<usize>() {
            let clamped = mb.clamp(MIN_LIMIT_MB, MAX_LIMIT_MB);
            LIMIT_BYTES.store(clamped * 1024 * 1024, Ordering::Relaxed);
        }
    }
}

/// Returns `(current_kb, peak_kb, limit_kb)`.
pub fn heap_stats_kb() -> (usize, usize, usize) {
    let cur = CURRENT_BYTES.load(Ordering::Relaxed) / 1024;
    let peak = PEAK_BYTES.load(Ordering::Relaxed) / 1024;
    let limit = LIMIT_BYTES.load(Ordering::Relaxed) / 1024;
    (cur, peak, limit)
}

/// Returns the configured hard heap ceiling in bytes.
pub fn hard_limit_bytes() -> usize {
    LIMIT_BYTES.load(Ordering::Relaxed)
}

/// True when live heap usage reaches or exceeds 70% of the configured hard limit.
pub fn is_near_soft_limit() -> bool {
    let cur = CURRENT_BYTES.load(Ordering::Relaxed);
    let limit = LIMIT_BYTES.load(Ordering::Relaxed);
    cur >= (limit / 10) * 7
}

#[inline]
fn try_reserve(size: usize) -> bool {
    let limit = LIMIT_BYTES.load(Ordering::Relaxed);
    let mut cur = CURRENT_BYTES.load(Ordering::Relaxed);
    loop {
        let Some(next) = cur.checked_add(size) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match CURRENT_BYTES.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                PEAK_BYTES.fetch_max(next, Ordering::Relaxed);
                return true;
            }
            Err(actual) => cur = actual,
        }
    }
}

#[inline]
fn release(size: usize) {
    let _ = CURRENT_BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        Some(cur.saturating_sub(size))
    });
}

unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if !try_reserve(size) {
            return ptr::null_mut();
        }
        // SAFETY: Delegating valid `layout` directly to `std::alloc::System`.
        let p = unsafe { System.alloc(layout) };
        if p.is_null() {
            release(size);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        if !try_reserve(size) {
            return ptr::null_mut();
        }
        // SAFETY: Delegating valid `layout` directly to `std::alloc::System`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if p.is_null() {
            release(size);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` and `layout` originate from this allocator.
        unsafe { System.dealloc(ptr, layout) };
        release(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old_size = layout.size();
        if new_size > old_size {
            let delta = new_size - old_size;
            if !try_reserve(delta) {
                return ptr::null_mut();
            }
            // SAFETY: Delegating valid `realloc` parameters to `std::alloc::System`.
            let p = unsafe { System.realloc(ptr, layout, new_size) };
            if p.is_null() {
                release(delta);
            }
            p
        } else {
            // SAFETY: Shrinking allocation via `std::alloc::System`.
            let p = unsafe { System.realloc(ptr, layout, new_size) };
            // A null realloc leaves the old block live, so dealloc will release old_size.
            if !p.is_null() {
                release(old_size - new_size);
            }
            p
        }
    }
}
