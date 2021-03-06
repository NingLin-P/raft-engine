use std::io::BufRead;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use std::{cmp, fmt, mem, u64};

use protobuf::Message as PbMsg;
use raft::eraftpb::Entry;

use crate::util::{HashMap, HashSet, RAFT_LOG_STATE_KEY};

use crate::config::Config;
use crate::log_batch::{
    self, Command, CompressionType, LogBatch, LogItemType, OpType, CHECKSUM_LEN, HEADER_LEN,
};
use crate::memtable::{EntryIndex, MemTable};
use crate::metrics::*;
use crate::pipe_log::{PipeLog, FILE_MAGIC_HEADER, VERSION};
use crate::{codec, CacheStats, RaftEngine, RaftLocalState, Result};

const SLOTS_COUNT: usize = 128;

#[derive(Clone, Copy, Debug)]
#[repr(i32)]
pub enum RecoveryMode {
    TolerateCorruptedTailRecords = 0,
    AbsoluteConsistency = 1,
}

impl From<i32> for RecoveryMode {
    fn from(i: i32) -> RecoveryMode {
        assert!(
            RecoveryMode::TolerateCorruptedTailRecords as i32 <= i
                && i <= RecoveryMode::AbsoluteConsistency as i32
        );
        unsafe { mem::transmute(i) }
    }
}

struct FileEngineInner {
    cfg: Config,

    // Multiple slots
    // region_id -> MemTable.
    memtables: Vec<RwLock<HashMap<u64, MemTable>>>,

    // Persistent entries.
    pipe_log: PipeLog,

    cache_stats: Arc<SharedCacheStats>,
}

impl FileEngineInner {
    // recover from disk.
    fn recover(&mut self, recovery_mode: RecoveryMode) -> Result<()> {
        // Get first file number and last file number.
        let (first_file_num, active_file_num) = {
            (
                self.pipe_log.first_file_num(),
                self.pipe_log.active_file_num(),
            )
        };

        let start = Instant::now();

        // Iterate files one by one
        let mut current_read_file = first_file_num;
        loop {
            if current_read_file > active_file_num {
                break;
            }

            // Read a file
            let content = {
                self.pipe_log
                    .read_next_file()
                    .unwrap_or_else(|e| {
                        panic!(
                            "Read content of file {} failed, error {:?}",
                            current_read_file, e
                        )
                    })
                    .unwrap_or_else(|| panic!("Expect has content, but get None"))
            };

            // Verify file header
            let mut buf = content.as_slice();
            if buf.len() < FILE_MAGIC_HEADER.len() || !buf.starts_with(FILE_MAGIC_HEADER) {
                if current_read_file != active_file_num {
                    panic!("Raft log file {} is corrupted.", current_read_file);
                } else {
                    self.pipe_log.truncate_active_log(0).unwrap();
                    break;
                }
            }

            // Iterate all LogBatch in one file
            let start_ptr = buf.as_ptr();
            buf.consume(FILE_MAGIC_HEADER.len() + VERSION.len());
            let mut offset = (FILE_MAGIC_HEADER.len() + VERSION.len()) as u64;
            loop {
                match LogBatch::from_bytes(&mut buf, current_read_file, offset) {
                    Ok(Some(log_batch)) => {
                        self.apply_to_memtable(log_batch, current_read_file);
                        offset = (buf.as_ptr() as usize - start_ptr as usize) as u64;
                    }
                    Ok(None) => {
                        info!("Recovered raft log file {}.", current_read_file);
                        break;
                    }
                    Err(e) => {
                        // There may be a pre-allocated space at the tail of the active log.
                        if current_read_file == active_file_num {
                            match recovery_mode {
                                RecoveryMode::TolerateCorruptedTailRecords => {
                                    warn!(
                                        "Encounter err {:?}, incomplete batch in last log file {}, \
                                         offset {}, truncate it in TolerateCorruptedTailRecords \
                                         recovery mode.",
                                        e,
                                        current_read_file,
                                        offset
                                    );
                                    self.pipe_log.truncate_active_log(offset as usize).unwrap();
                                    break;
                                }
                                RecoveryMode::AbsoluteConsistency => {
                                    panic!(
                                        "Encounter err {:?}, incomplete batch in last log file {}, \
                                         offset {}, panic in AbsoluteConsistency recovery mode.",
                                        e,
                                        current_read_file,
                                        offset
                                    );
                                }
                            }
                        } else {
                            panic!("Corruption occur in middle log file {}", current_read_file);
                        }
                    }
                }
            }

            // Only keep latest entries in cache, keep cache below limited size.
            if self.cfg.cache_size_limit.0 > 0
                && (current_read_file - first_file_num) * self.cfg.target_file_size.0
                    > self.cfg.cache_size_limit.0
            {
                let total_files_in_cache =
                    self.cfg.cache_size_limit.0 / self.cfg.target_file_size.0;
                if current_read_file > total_files_in_cache {
                    for memtables in &self.memtables {
                        let mut memtables = memtables.write().unwrap();
                        for memtable in memtables.values_mut() {
                            memtable.evict_old_from_cache(current_read_file - total_files_in_cache);
                        }
                    }
                }
            }

            current_read_file += 1;
        }

        info!("Recover raft log takes {:?}", start.elapsed());

        Ok(())
    }

    fn apply_to_memtable(&self, log_batch: LogBatch, file_num: u64) {
        for item in log_batch.items.borrow_mut().drain(..) {
            match item.item_type {
                LogItemType::Entries => {
                    let entries_to_add = item.entries.unwrap();
                    let region_id = entries_to_add.region_id;
                    let mut memtables = self.memtables[region_id as usize % SLOTS_COUNT]
                        .write()
                        .unwrap();
                    let memtable = memtables.entry(region_id).or_insert_with(|| {
                        let cache_limit = self.cfg.region_size.0 / 2;
                        let cache_stats = self.cache_stats.clone();
                        MemTable::new(region_id, cache_limit, cache_stats)
                    });
                    memtable.append(
                        entries_to_add.entries,
                        entries_to_add.entries_index.into_inner(),
                    );
                }
                LogItemType::CMD => {
                    let command = item.command.unwrap();
                    match command {
                        Command::Clean { region_id } => {
                            let mut memtables = self.memtables[region_id as usize % SLOTS_COUNT]
                                .write()
                                .unwrap();
                            memtables.remove(&region_id);
                        }
                    }
                }
                LogItemType::KV => {
                    let kv = item.kv.unwrap();
                    let mut memtables = self.memtables[kv.region_id as usize % SLOTS_COUNT]
                        .write()
                        .unwrap();
                    let memtable = memtables.entry(kv.region_id).or_insert_with(|| {
                        let cache_limit = self.cfg.region_size.0 / 2;
                        let stats = self.cache_stats.clone();
                        MemTable::new(kv.region_id, cache_limit, stats)
                    });
                    match kv.op_type {
                        OpType::Put => {
                            memtable.put(kv.key, kv.value.unwrap(), file_num);
                        }
                        OpType::Del => {
                            memtable.delete(kv.key.as_slice());
                        }
                    }
                }
            }
        }
    }

    // Rewrite inactive region's entries and key/value pairs,
    // so the old files can be dropped ASAP.
    #[allow(dead_code)]
    fn rewrite_inactive(&self) -> bool {
        let inactive_file_num = {
            self.pipe_log
                .files_before(self.cfg.cache_size_limit.0 as usize)
        };

        if inactive_file_num == 0 {
            return false;
        }

        let mut has_write = false;
        let mut memory_usage = 0;
        for slot in 0..SLOTS_COUNT {
            let mut memtables = self.memtables[slot].write().unwrap();
            for memtable in memtables.values_mut() {
                memory_usage += memtable.entries_size();

                let min_file_num = match memtable.min_file_num() {
                    Some(file_num) => file_num,
                    None => continue,
                };

                // Has no entry in inactive files, skip.
                if min_file_num >= inactive_file_num {
                    continue;
                }

                // Has entries in inactive files, at the same time the total entries is less
                // than `compact_threshold`, compaction will not be triggered, so we need rewrite
                // these entries, so the old files can be dropped ASAP.
                if memtable.entries_count() < self.cfg.compact_threshold {
                    REWRITE_COUNTER.inc();
                    REWRITE_ENTRIES_COUNT_HISTOGRAM.observe(memtable.entries_count() as f64);
                    has_write = true;

                    // Dump all entries
                    // Not all entries are in cache always, we may need read remains
                    // entries from file.
                    let mut ents = Vec::with_capacity(memtable.entries_count());
                    let mut ents_idx = Vec::with_capacity(memtable.entries_count());
                    memtable.fetch_all(&mut ents, &mut ents_idx);
                    let mut all_ents = Vec::with_capacity(memtable.entries_count());
                    for i in ents_idx {
                        let e = self.read_entry_from_file(&i).unwrap_or_else(|e| {
                            panic!(
                                "Read entry from file {} at offset {} failed \
                                     when rewriting, err {:?}",
                                i.file_num, i.offset, e
                            )
                        });
                        all_ents.push(e);
                    }
                    all_ents.extend(ents.into_iter());
                    let log_batch = LogBatch::new();
                    log_batch.add_entries(memtable.region_id(), all_ents);

                    // Dump all key value pairs
                    let mut kvs = vec![];
                    memtable.fetch_all_kvs(&mut kvs);
                    for kv in &kvs {
                        log_batch.put(memtable.region_id(), &kv.0, &kv.1);
                    }

                    // Rewrite to new log file
                    let mut file_num = 0;
                    self.pipe_log
                        .append_log_batch(&log_batch, false, &mut file_num)
                        .unwrap();

                    // Apply to memtable.
                    // FIXME: using slef.apply_to_memtable here will cause deadlock.
                    for item in log_batch.items.borrow_mut().drain(..) {
                        match item.item_type {
                            LogItemType::Entries => {
                                let entries_to_add = item.entries.unwrap();
                                assert_eq!(entries_to_add.region_id, memtable.region_id());
                                memtable.append(
                                    entries_to_add.entries,
                                    entries_to_add.entries_index.into_inner(),
                                );
                            }
                            LogItemType::CMD => {
                                panic!("Memtable doesn't contain command item.");
                            }
                            LogItemType::KV => {
                                let kv = item.kv.unwrap();
                                assert_eq!(kv.region_id, memtable.region_id());
                                match kv.op_type {
                                    OpType::Put => {
                                        memtable.put(kv.key, kv.value.unwrap(), file_num);
                                    }
                                    OpType::Del => {
                                        memtable.delete(kv.key.as_slice());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        RAFTENGINE_MEMORY_USAGE_GAUGE.set(memory_usage as f64);

        has_write
    }

    #[allow(dead_code)]
    fn regions_need_force_compact(&self) -> HashSet<u64> {
        // first_file_num: the oldest file number.
        // current_file_num: current file number.
        // inactive_file_num: files before this one should not in cache.
        // gc_file_num: entries in these files should compact by force.
        let (inactive_file_num, gc_file_num) = {
            (
                self.pipe_log
                    .files_before(self.cfg.cache_size_limit.0 as usize),
                self.pipe_log
                    .files_before(self.cfg.total_size_limit.0 as usize),
            )
        };

        let mut regions = HashSet::default();
        let region_entries_size_limit = self.cfg.region_size.0 * 2 / 3;
        for slot in 0..SLOTS_COUNT {
            let memtables = self.memtables[slot].read().unwrap();
            for memtable in memtables.values() {
                // Total size of entries for this region exceed limit.
                if memtable.entries_size() > region_entries_size_limit {
                    info!(
                        "region {}'s total raft log size {} exceed limit \
                         need force compaction",
                        memtable.region_id(),
                        memtable.entries_size(),
                    );
                    regions.insert(memtable.region_id());
                    continue;
                }

                if inactive_file_num == 0 || gc_file_num == 0 {
                    continue;
                }

                let min_file_num = match memtable.min_file_num() {
                    Some(file_num) => file_num,
                    None => continue,
                };

                // Has entries left behind too far, this happens when
                // some followers left behind for a long time.
                if min_file_num < gc_file_num {
                    info!(
                        "region {}'s some followers left behind too far, \
                         need force compaction",
                        memtable.region_id()
                    );
                    regions.insert(memtable.region_id());
                }
            }
        }
        NEED_COMPACT_REGIONS_HISTOGRAM.observe(regions.len() as f64);

        regions
    }

    #[allow(dead_code)]
    fn evict_old_from_cache(&self) {
        let inactive_file_num = self
            .pipe_log
            .files_before(self.cfg.cache_size_limit.0 as usize);

        if inactive_file_num == 0 {
            return;
        }

        for slot in 0..SLOTS_COUNT {
            let mut memtables = self.memtables[slot].write().unwrap();
            for memtable in memtables.values_mut() {
                memtable.evict_old_from_cache(inactive_file_num);
            }
        }
    }

    #[allow(dead_code)]
    fn purge_expired_files(&self) -> Result<()> {
        let mut min_file_num = u64::MAX;
        for memtables in &self.memtables {
            let memtables = memtables.read().unwrap();
            let file_num = memtables.values().fold(u64::MAX, |min, x| {
                cmp::min(min, x.min_file_num().map_or(u64::MAX, |num| num))
            });
            if file_num < min_file_num {
                min_file_num = file_num;
            }
        }

        self.pipe_log.purge_to(min_file_num)
    }

    fn compact_to(&self, region_id: u64, index: u64) -> u64 {
        let mut memtables = self.memtables[region_id as usize % SLOTS_COUNT]
            .write()
            .unwrap();
        if let Some(memtable) = memtables.get_mut(&region_id) {
            return memtable.compact_to(index);
        }
        0
    }

    fn compact_cache_to(&self, region_id: u64, index: u64) {
        let mut memtables = self.memtables[region_id as usize % SLOTS_COUNT]
            .write()
            .unwrap();
        if let Some(memtable) = memtables.get_mut(&region_id) {
            memtable.compact_cache_to(index);
        }
    }

    fn write(&self, log_batch: LogBatch, sync: bool) -> Result<usize> {
        let mut file_num = 0;
        let bytes = self
            .pipe_log
            .append_log_batch(&log_batch, sync, &mut file_num)?;
        self.post_append_to_file(log_batch, file_num);
        Ok(bytes)
    }

    fn sync(&self) -> Result<()> {
        self.pipe_log.sync();
        Ok(())
    }

    #[allow(dead_code)]
    fn kv_count(&self, region_id: u64) -> usize {
        let memtables = self.memtables[region_id as usize % SLOTS_COUNT]
            .read()
            .unwrap();
        if let Some(memtable) = memtables.get(&region_id) {
            return memtable.kvs_total_count();
        }
        0
    }

    fn put_msg<M: protobuf::Message>(&self, region_id: u64, key: &[u8], m: &M) -> Result<()> {
        let log_batch = LogBatch::new();
        log_batch.put_msg(region_id, key, m)?;
        self.write(log_batch, false).map(|_| ())
    }

    fn get(&self, region_id: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let memtables = self.memtables[region_id as usize % SLOTS_COUNT]
            .read()
            .unwrap();
        if let Some(memtable) = memtables.get(&region_id) {
            Ok(memtable.get(key))
        } else {
            Ok(None)
        }
    }

    fn get_msg<M: protobuf::Message>(&self, region_id: u64, key: &[u8]) -> Result<Option<M>> {
        match self.get(region_id, key)? {
            Some(value) => {
                let mut m = M::new();
                m.merge_from_bytes(&value)?;
                Ok(Some(m))
            }
            None => Ok(None),
        }
    }

    fn get_entry(&self, region_id: u64, log_idx: u64) -> Result<Option<Entry>> {
        // Fetch from cache
        let entry_idx = {
            let memtables = self.memtables[region_id as usize % SLOTS_COUNT]
                .read()
                .unwrap();
            if let Some(memtable) = memtables.get(&region_id) {
                match memtable.get_entry(log_idx) {
                    (Some(entry), _) => return Ok(Some(entry)),
                    (None, Some(idx)) => idx,
                    (None, None) => return Ok(None),
                }
            } else {
                return Ok(None);
            }
        };

        // Read from file
        let entry = self.read_entry_from_file(&entry_idx).unwrap_or_else(|e| {
            panic!(
                "Read entry from file for region {} index {} failed, err {:?}",
                region_id, log_idx, e
            )
        });
        Ok(Some(entry))
    }

    fn read_entry_from_file(&self, entry_index: &EntryIndex) -> Result<Entry> {
        let file_num = entry_index.file_num;
        let base_offset = entry_index.base_offset;
        let batch_len = entry_index.batch_len;
        let offset = entry_index.offset;
        let len = entry_index.len;

        let entry_content = match entry_index.compression_type {
            CompressionType::None => {
                let offset = base_offset + offset;
                self.pipe_log.fread(file_num, offset, len)?
            }
            CompressionType::Lz4 => {
                let read_len = batch_len + 8; // 8 bytes for header.
                let compressed = self.pipe_log.fread(file_num, base_offset, read_len)?;
                let mut reader = compressed.as_ref();
                let header = codec::decode_u64(&mut reader)?;
                assert_eq!(header >> 8, batch_len);

                log_batch::test_batch_checksum(reader)?;
                let buf = log_batch::decompress(&reader[..batch_len as usize - CHECKSUM_LEN]);
                let start = offset as usize - HEADER_LEN;
                let end = (offset + len) as usize - HEADER_LEN;
                buf[start..end].to_vec()
            }
        };

        let mut e = Entry::new();
        e.merge_from_bytes(&entry_content)?;
        assert_eq!(e.get_index(), entry_index.index);
        Ok(e)
    }

    pub fn fetch_entries_to(
        &self,
        region_id: u64,
        begin: u64,
        end: u64,
        max_size: Option<usize>,
        vec: &mut Vec<Entry>,
    ) -> Result<usize> {
        let memtables = self.memtables[region_id as usize % SLOTS_COUNT]
            .read()
            .unwrap();
        if let Some(memtable) = memtables.get(&region_id) {
            let mut entries = Vec::with_capacity((end - begin) as usize);
            let mut entries_idx = Vec::with_capacity((end - begin) as usize);
            memtable.fetch_entries_to(begin, end, max_size, &mut entries, &mut entries_idx)?;
            let count = entries.len() + entries_idx.len();
            for idx in &entries_idx {
                let e = self.read_entry_from_file(idx)?;
                vec.push(e);
            }
            vec.extend(entries.into_iter());
            return Ok(count);
        }
        Ok(0)
    }

    fn post_append_to_file(&self, log_batch: LogBatch, file_num: u64) {
        // 0 means write nothing.
        if file_num == 0 {
            return;
        }
        self.apply_to_memtable(log_batch, file_num);
    }
}

#[derive(Default)]
pub struct SharedCacheStats {
    hit: AtomicUsize,
    miss: AtomicUsize,
    mem_size_change: AtomicIsize,
}

impl SharedCacheStats {
    pub fn sub_mem_change(&self, bytes: u64) {
        self.mem_size_change
            .fetch_sub(bytes as isize, Ordering::Relaxed);
    }
    pub fn add_mem_change(&self, bytes: u64) {
        self.mem_size_change
            .fetch_add(bytes as isize, Ordering::Relaxed);
    }
    pub fn hit_cache(&self, count: usize) {
        self.hit.fetch_add(count, Ordering::Relaxed);
    }
    pub fn miss_cache(&self, count: usize) {
        self.miss.fetch_add(count, Ordering::Relaxed);
    }
    pub fn hit_times(&self) -> usize {
        self.hit.load(Ordering::Relaxed)
    }
    pub fn miss_times(&self) -> usize {
        self.miss.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub fn reset(&self) {
        self.hit.store(0, Ordering::Relaxed);
        self.miss.store(0, Ordering::Relaxed);
        self.mem_size_change.store(0, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct FileEngine {
    inner: Arc<FileEngineInner>,
}

impl fmt::Debug for FileEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileEngineInner dir: {}", self.inner.cfg.dir)
    }
}

impl FileEngine {
    pub fn new(cfg: Config) -> FileEngine {
        let cache_stats = Arc::new(SharedCacheStats::default());

        let pipe_log = PipeLog::open(
            &cfg.dir,
            cfg.bytes_per_sync.0 as usize,
            cfg.target_file_size.0 as usize,
        )
        .unwrap_or_else(|e| panic!("Open raft log failed, error: {:?}", e));
        let mut memtables = Vec::with_capacity(SLOTS_COUNT);
        for _ in 0..SLOTS_COUNT {
            memtables.push(RwLock::new(HashMap::default()));
        }
        let mut engine = FileEngineInner {
            cfg,
            memtables,
            pipe_log,
            cache_stats,
        };
        let recovery_mode = RecoveryMode::from(engine.cfg.recovery_mode);
        engine
            .recover(recovery_mode)
            .unwrap_or_else(|e| panic!("Recover raft log failed, error: {:?}", e));

        FileEngine {
            inner: Arc::new(engine),
        }
    }
}

impl RaftEngine for FileEngine {
    type LogBatch = LogBatch;

    fn log_batch(&self, _capacity: usize) -> Self::LogBatch {
        LogBatch::default()
    }

    fn sync(&self) -> Result<()> {
        self.inner.sync()
    }

    fn get_raft_state(&self, raft_group_id: u64) -> Result<Option<RaftLocalState>> {
        self.inner.get_msg(raft_group_id, RAFT_LOG_STATE_KEY)
    }

    fn get_entry(&self, raft_group_id: u64, index: u64) -> Result<Option<Entry>> {
        self.inner.get_entry(raft_group_id, index)
    }

    fn fetch_entries_to(
        &self,
        raft_group_id: u64,
        begin: u64,
        end: u64,
        max_size: Option<usize>,
        to: &mut Vec<Entry>,
    ) -> Result<usize> {
        self.inner
            .fetch_entries_to(raft_group_id, begin, end, max_size, to)
    }

    fn consume(&self, batch: &mut Self::LogBatch, sync: bool) -> Result<usize> {
        self.inner.write(std::mem::take(batch), sync)
    }

    fn consume_and_shrink(
        &self,
        batch: &mut Self::LogBatch,
        sync: bool,
        _: usize,
        _: usize,
    ) -> Result<usize> {
        self.consume(batch, sync)
    }

    fn clean(&self, raft_group_id: u64, _: &RaftLocalState, batch: &mut LogBatch) -> Result<()> {
        batch.clean_region(raft_group_id);
        Ok(())
    }

    fn append(&self, raft_group_id: u64, entries: Vec<Entry>) -> Result<usize> {
        let batch = LogBatch::default();
        batch.add_entries(raft_group_id, entries);
        self.inner.write(batch, false)
    }

    fn put_raft_state(&self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        self.inner.put_msg(raft_group_id, RAFT_LOG_STATE_KEY, state)
    }

    fn gc(&self, raft_group_id: u64, _from: u64, to: u64) -> Result<usize> {
        let entries = self.inner.compact_to(raft_group_id, to) as usize;
        Ok(entries)
    }

    fn has_builtin_entry_cache(&self) -> bool {
        true
    }

    fn gc_entry_cache(&self, raft_group_id: u64, to: u64) {
        self.inner.compact_cache_to(raft_group_id, to)
    }

    fn flush_stats(&self) -> CacheStats {
        let inner = &self.inner;
        CacheStats {
            hit: inner.cache_stats.hit.swap(0, Ordering::SeqCst),
            miss: inner.cache_stats.miss.swap(0, Ordering::SeqCst),
            mem_size_change: inner.cache_stats.mem_size_change.swap(0, Ordering::SeqCst),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_entry_from_file() {
        let normal_batch_size = 10;
        let compressed_batch_size = 5120;
        for &entry_size in &[normal_batch_size, compressed_batch_size] {
            let dir = tempfile::Builder::new()
                .prefix("test_engine")
                .tempdir()
                .unwrap();

            let mut cfg = Config::default();
            cfg.dir = dir.path().to_str().unwrap().to_owned();

            let engine = FileEngine::new(cfg.clone());
            let mut entry = Entry::new();
            entry.set_data(vec![b'x'; entry_size]);
            for i in 10..20 {
                entry.set_index(i);
                engine.append(i, vec![entry.clone()]).unwrap();
                entry.set_index(i + 1);
                engine.append(i, vec![entry.clone()]).unwrap();
            }

            for i in 10..20 {
                // Test get_entry from cache.
                entry.set_index(i + 1);
                assert_eq!(engine.get_entry(i, i + 1).unwrap(), Some(entry.clone()));

                // Test get_entry from file.
                entry.set_index(i);
                assert_eq!(engine.get_entry(i, i).unwrap(), Some(entry.clone()));
            }

            drop(engine);

            // Recover the engine.
            let engine = FileEngine::new(cfg.clone());
            for i in 10..20 {
                entry.set_index(i + 1);
                assert_eq!(engine.get_entry(i, i + 1).unwrap(), Some(entry.clone()));

                entry.set_index(i);
                assert_eq!(engine.get_entry(i, i).unwrap(), Some(entry.clone()));
            }
        }
    }
}
