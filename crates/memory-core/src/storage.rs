/// How aggressively to push WAL writes to durable storage before acknowledging
/// the client.
///
/// The default [`Async`](Durability::Async) flushes to the kernel page cache
/// and returns immediately; the background sync thread calls `fsync` within
/// ~1 second. Journal-mode filesystems (ext4, APFS, NTFS) typically absorb a
/// power loss within that window.
///
/// [`Sync`](Durability::Sync) calls `fsync` before returning, confirming the
/// data is on stable media. Use this when every write must survive an immediate
/// power failure, at the cost of higher write latency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    Async,
    Sync,
}

impl Durability {
    pub const fn is_sync(self) -> bool {
        matches!(self, Durability::Sync)
    }
}

impl std::str::FromStr for Durability {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "async" | "Async" => Ok(Durability::Async),
            "sync" | "Sync" => Ok(Durability::Sync),
            _ => Err(format!(
                "unknown durability '{s}'; expected 'async' or 'sync'"
            )),
        }
    }
}

/// Tunable SQLite pragmas applied when opening the database. `page_size` and
/// `auto_vacuum` only take effect on a freshly-created database (they are fixed
/// once the file has content); the rest apply on every open. Defaults target a
/// Linux host (4 KiB pages match the OS page / filesystem block size).
#[derive(Debug, Clone, Copy)]
pub struct SqliteTuning {
    /// `PRAGMA mmap_size` in bytes.
    pub mmap_size: i64,
    /// `PRAGMA page_size` in bytes (fresh DB only). Must be a power of two.
    pub page_size: i64,
    /// `PRAGMA cache_size` magnitude in KiB (applied as the negative form).
    pub cache_size_kb: i64,
    /// `PRAGMA busy_timeout` in milliseconds.
    pub busy_timeout_ms: u64,
    /// `PRAGMA journal_size_limit` in bytes.
    pub journal_size_limit: i64,
}

impl Default for SqliteTuning {
    fn default() -> Self {
        Self {
            mmap_size: 268_435_456, // 256 MiB
            page_size: 4096,        // 4 KiB — matches Linux page/fs block
            cache_size_kb: 50_000,  // ~50 MiB
            busy_timeout_ms: 5000,
            journal_size_limit: 134_217_728, // 128 MiB
        }
    }
}
