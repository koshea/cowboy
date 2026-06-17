//! Time helpers. Timestamps across cowboy are **`u64` milliseconds since the
//! Unix epoch** (see AGENTS.md) — use [`now_ms`] everywhere rather than
//! re-deriving it, so the unit and clock source stay consistent.

/// Milliseconds since the Unix epoch (0 if the clock is before the epoch).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
