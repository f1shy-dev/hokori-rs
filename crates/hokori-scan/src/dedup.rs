use ahash::AHashSet;
use parking_lot::Mutex;

// PERF: 128 shards is well-tuned for 8-32 threads. With 3.4M entries and the nlink
// fast path (which skips dedup for nlink<=1, ~95% of files), only ~170K entries hit
// the sharded map. At 128 shards that's ~1.3K entries per shard, with very low
// contention since workers process different directories.
// The hash constant 0x517cc1b727220a95 is a known-good multiplicative hash from
// splitmix64. On single-filesystem scans (common case), dev is constant so the hash
// degenerates to (const ^ ino) % 128 — this is fine because ext4/xfs inode numbers
// are sequential and XOR with a constant preserves uniform distribution mod 128.
const SHARD_COUNT: usize = 128;

pub struct InodeDedup {
    shards: Box<[Mutex<AHashSet<(u64, u64)>>]>,
}

impl InodeDedup {
    pub fn new() -> Self {
        let shards: Vec<_> = (0..SHARD_COUNT)
            .map(|_| Mutex::new(AHashSet::new()))
            .collect();
        Self {
            shards: shards.into_boxed_slice(),
        }
    }

    pub fn check_and_insert(&self, dev: u64, ino: u64) -> bool {
        let hash = (dev.wrapping_mul(0x517cc1b727220a95)) ^ ino;
        let shard_idx = (hash as usize) % SHARD_COUNT;
        let mut shard = self.shards[shard_idx].lock();
        shard.insert((dev, ino))
    }

    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for InodeDedup {
    fn default() -> Self {
        Self::new()
    }
}
