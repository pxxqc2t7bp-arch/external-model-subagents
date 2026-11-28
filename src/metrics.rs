//! Fixed-cardinality process counters; snapshots are approximate during concurrent updates.
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static START: OnceLock<Instant> = OnceLock::new();
// Dispatch: total, success, fail, timeout. Routing: automatic, pass-through.
static DISPATCH: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static LATENCY: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
static ROUTING: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
// Pairs of started/failed counters for panel, pipeline, debate, respectively.
static SWARM: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Start the uptime clock once; never reset counters.
pub fn init() {
    START.get_or_init(Instant::now);
}

pub fn dispatch_started() {
    init();
    DISPATCH[0].fetch_add(1, Relaxed);
}

/// Timeouts are failures, even if the caller supplies `failed = false`.
pub fn dispatch_finished(elapsed: Duration, failed: bool, timed_out: bool) {
    init();
    DISPATCH[if failed || timed_out { 2 } else { 1 }].fetch_add(1, Relaxed);
    if timed_out {
        DISPATCH[3].fetch_add(1, Relaxed);
    }
    // Exactly one noncumulative bucket per completion, including failed dispatches.
    let bucket = match elapsed.as_secs() {
        0 => 0, 1..=4 => 1, 5..=14 => 2, 15..=59 => 3, _ => 4,
    };
    LATENCY[bucket].fetch_add(1, Relaxed);
}

pub fn routing(automatic: bool) {
    init();
    ROUTING[usize::from(!automatic)].fetch_add(1, Relaxed);
}

#[derive(Clone, Copy)]
pub enum SwarmKind {
    Panel = 0,
    Pipeline = 2,
    Debate = 4,
}

impl SwarmKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Panel => "panel",
            Self::Pipeline => "pipeline",
            Self::Debate => "debate",
        }
    }
}

pub fn swarm_started(kind: SwarmKind) {
    init();
    SWARM[kind as usize].fetch_add(1, Relaxed);
}

pub fn swarm_finished(kind: SwarmKind, failed: bool) {
    init();
    if failed {
        SWARM[kind as usize + 1].fetch_add(1, Relaxed);
    }
}

fn counts<const N: usize>(counters: &[AtomicU64; N]) -> [u64; N] {
    std::array::from_fn(|index| counters[index].load(Relaxed))
}

/// Fixed-size JSON shape; no samples, labels, or snapshots are retained in memory.
pub fn snapshot() -> Value {
    let uptime = START.get_or_init(Instant::now).elapsed().as_secs();
    let dispatch = counts(&DISPATCH);
    let latency = counts(&LATENCY);
    let routing = counts(&ROUTING);
    let swarm = counts(&SWARM);
    let (current, peak, limit) = crate::alloc_cap::heap_stats_kb();
    json!({
        "uptime_secs": uptime,
        "heap_stats_kb": { "current": current, "peak": peak, "limit": limit },
        "dispatch": {
            "total": dispatch[0], "success": dispatch[1],
            "fail": dispatch[2], "timeout": dispatch[3],
            "latency_buckets": {
                "lt_1s": latency[0], "lt_5s": latency[1], "lt_15s": latency[2],
                "lt_60s": latency[3], "gte_60s": latency[4]
            }
        },
        "routing": { "automatic": routing[0], "pass_through": routing[1] },
        "swarm": {
            "panel": { "count": swarm[0], "fail": swarm[1] },
            "pipeline": { "count": swarm[2], "fail": swarm[3] },
            "debate": { "count": swarm[4], "fail": swarm[5] }
        }
    })
}
