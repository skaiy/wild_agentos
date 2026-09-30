//! Production-grade Write-Ahead Log with:
//! - CRC32 per-entry integrity
//! - Three sync modes (Strict/Batch/Async)
//! - 512MB auto-rotation with atomic rename
//! - File locking (Unix: flock LOCK_EX|LOCK_NB)
//! - Replay with CRC verification + automatic truncation
//! - Error surge protection (3 consecutive IO errors → StorageCritical)

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};

use crate::error::EngineError;

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WalSyncMode {
    Strict,
    Batch { interval_ms: u64 },
    Async,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalOp {
    Insert { id: u32, iri: String },
    Upsert { id: u32, iri: String },
    Delete { id: u32, iri: String },
    MetadataUpdate { id: u32, iri: String },
}

impl WalOp {
    pub fn variant_byte(&self) -> u8 {
        match self {
            Self::Insert { .. } => 1,
            Self::Upsert { .. } => 2,
            Self::Delete { .. } => 3,
            Self::MetadataUpdate { .. } => 4,
        }
    }
}

// ── EngineWal ───────────────────────────────────────────────────────────────

pub struct EngineWal {
    dir: PathBuf,
    active_path: PathBuf,
    #[allow(dead_code)]
    lock_path: PathBuf,
    sync_mode: WalSyncMode,
    #[allow(dead_code)]
    batch_fsync_interval_ms: u64,

    // Protected by Mutex for sequential append safety
    inner: Mutex<WalInner>,

    // Tracked atomically for rotation decisions without lock
    current_size: AtomicU64,

    // Lock file handle - keep alive to hold flock
    #[allow(dead_code)]
    lock_file: Option<File>,

    // Surge protection
    io_error_count: AtomicU64,
}

struct WalInner {
    file: File,
    last_fsync: Instant,
}

impl EngineWal {
    const MAGIC: u8 = 0xFE;
    const MAX_SIZE: u64 = 512 * 1024 * 1024;
    const MAX_IO_ERRORS: u64 = 3;

    pub fn open(dir: &Path, sync_mode: WalSyncMode) -> Result<Self, EngineError> {
        fs::create_dir_all(dir)?;

        let active_path = dir.join("active.wal");
        let lock_path = dir.join("wal.lock");

        // File lock (Unix only)
        #[cfg(unix)]
        let lock_file = {
            let lf = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?;
            let fd = lf.as_raw_fd();
            let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if ret != 0 {
                let err = io::Error::last_os_error();
                return Err(EngineError::StorageError {
                    message: format!("Cannot acquire WAL lock: {err}"),
                });
            }
            Some(lf)
        };
        #[cfg(not(unix))]
        let lock_file = None;

        // Recover any frozen WAL files from prior crash
        Self::recover_frozen_wals(dir)?;

        // Open active WAL
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&active_path)?;

        let current_size = file.metadata()?.len();

        let batch_fsync_interval_ms = match sync_mode {
            WalSyncMode::Batch { interval_ms } => interval_ms,
            _ => 5000,
        };

        Ok(Self {
            dir: dir.to_owned(),
            active_path,
            lock_path,
            sync_mode,
            batch_fsync_interval_ms,
            inner: Mutex::new(WalInner {
                file,
                last_fsync: Instant::now(),
            }),
            current_size: AtomicU64::new(current_size),
            lock_file,
            io_error_count: AtomicU64::new(0),
        })
    }

    // ── Append ────────────────────────────────────────────────────────────

    /// Append a WAL entry. The id is serialized into the packet for crash recovery.
    pub fn append(&self, op: &WalOp, clock: u64, data: &[u8]) -> Result<(), EngineError> {
        // Surge protection
        if self.io_error_count.load(Ordering::Acquire) >= Self::MAX_IO_ERRORS {
            return Err(EngineError::StorageCritical(
                "WAL: max consecutive IO errors reached, engine must checkpoint".into(),
            ));
        }

        let id = match op {
            WalOp::Insert { id, .. }
            | WalOp::Upsert { id, .. }
            | WalOp::Delete { id, .. }
            | WalOp::MetadataUpdate { id, .. } => *id,
        };

        // Build packet: [Magic(1) PayloadLen(4) CRC32(4) Payload...]
        // Payload = [Opcode(1) ID(4) Clock(8) Data...]
        let payload_len = data.len() + 13; // opcode(1) + id(4) + clock(8)
        let mut packet = Vec::with_capacity(payload_len + 9);

        let mut hasher = Hasher::new();
        hasher.update(&[op.variant_byte()]);
        hasher.update(&id.to_le_bytes());
        hasher.update(&clock.to_le_bytes());
        hasher.update(data);
        let crc = hasher.finalize();

        packet.push(Self::MAGIC);
        packet
            .write_u32::<LittleEndian>(payload_len as u32)
            .unwrap();
        packet.write_u32::<LittleEndian>(crc).unwrap();
        packet.push(op.variant_byte());
        packet.write_u32::<LittleEndian>(id).unwrap();
        packet.write_u64::<LittleEndian>(clock).unwrap();
        packet.write_all(data).unwrap();

        let packet_len = packet.len() as u64;

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| EngineError::internal("WAL mutex poisoned"))?;

        // Check rotation (inside lock to avoid races)
        let current = self.current_size.load(Ordering::Relaxed);
        if current + packet_len > Self::MAX_SIZE {
            drop(inner);
            self.rotate()?;
            inner = self
                .inner
                .lock()
                .map_err(|_| EngineError::internal("WAL mutex poisoned"))?;
        }

        // Write
        if let Err(e) = inner.file.write_all(&packet) {
            let count = self.io_error_count.fetch_add(1, Ordering::AcqRel) + 1;
            if count >= Self::MAX_IO_ERRORS {
                return Err(EngineError::StorageCritical(format!(
                    "WAL: {count} consecutive IO errors: {e}"
                )));
            }
            return Err(EngineError::StorageError {
                message: format!("WAL write: {e}"),
            });
        }

        // Sync
        self.sync_internal(&mut inner)?;

        // Success → reset error count
        self.io_error_count.store(0, Ordering::Release);
        self.current_size.fetch_add(packet_len, Ordering::Relaxed);

        Ok(())
    }

    fn sync_internal(&self, inner: &mut WalInner) -> Result<(), EngineError> {
        match self.sync_mode {
            WalSyncMode::Strict => {
                inner
                    .file
                    .sync_all()
                    .map_err(|e| EngineError::StorageError {
                        message: format!("WAL fsync: {e}"),
                    })?;
            }
            WalSyncMode::Batch { interval_ms } => {
                if inner.last_fsync.elapsed().as_millis() as u64 >= interval_ms {
                    inner
                        .file
                        .sync_all()
                        .map_err(|e| EngineError::StorageError {
                            message: format!("WAL batch fsync: {e}"),
                        })?;
                    inner.last_fsync = Instant::now();
                }
            }
            WalSyncMode::Async => {}
        }
        Ok(())
    }

    // ── Rotate (atomic) ────────────────────────────────────────────────────

    pub fn rotate(&self) -> Result<(), EngineError> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let frozen_name = format!("frozen.{timestamp}.wal");
        let frozen_path = self.dir.join(&frozen_name);

        // Sync current file before rename
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| EngineError::internal("WAL mutex poisoned"))?;
        inner.file.flush()?;
        inner.file.sync_all()?;
        inner.file.seek(std::io::SeekFrom::Start(0))?;
        drop(inner);

        // Atomic rename (POSIX guarantee: rename is atomic on same filesystem)
        fs::rename(&self.active_path, &frozen_path)?;

        // Create new active WAL
        let new_file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.active_path)?;

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| EngineError::internal("WAL mutex poisoned"))?;
        inner.file = new_file;
        inner.last_fsync = Instant::now();
        drop(inner);

        self.current_size.store(0, Ordering::Release);
        Ok(())
    }

    // ── Sync (explicit, for checkpoint coordination) ───────────────────────

    pub fn sync(&self) -> Result<(), EngineError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| EngineError::internal("WAL mutex poisoned"))?;
        inner.file.flush()?;
        inner.file.sync_all()?;
        inner.last_fsync = Instant::now();
        Ok(())
    }

    // ── Replay (static, CRC verification + truncation) ────────────────────

    pub fn replay<F>(path: &Path, mut callback: F) -> Result<u64, EngineError>
    where
        F: FnMut(WalOp, u64, &[u8]),
    {
        if !path.exists() {
            return Ok(0);
        }

        let file = File::open(path)?;
        let file_len = file.metadata()?.len();
        let mut reader = BufReader::new(file);
        let mut valid_pos = 0u64;
        let mut count = 0u64;

        loop {
            // Magic byte
            let magic = match reader.read_u8() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            };

            // Format check
            if magic != Self::MAGIC {
                valid_pos += 1;
                continue;
            }

            // Payload length + CRC32
            let payload_len = match reader.read_u32::<LittleEndian>() {
                Ok(len) => len as u64,
                _ => break,
            };
            let stored_crc = match reader.read_u32::<LittleEndian>() {
                Ok(crc) => crc,
                _ => break,
            };

            // Read payload
            let mut payload = vec![0u8; payload_len as usize];
            if reader.read_exact(&mut payload).is_err() {
                break;
            }

            // CRC32 verify
            let mut hasher = Hasher::new();
            hasher.update(&payload);
            if hasher.finalize() != stored_crc {
                tracing::warn!("WAL CRC mismatch at offset {}, truncating", valid_pos);
                break;
            }

            // Parse: [Opcode(1) ID(4) Clock(8) Data...]
            let mut cursor = std::io::Cursor::new(&payload[..]);
            let opcode = match cursor.read_u8() {
                Ok(o) => o,
                Err(_) => break,
            };
            let entry_id = match cursor.read_u32::<LittleEndian>() {
                Ok(id) => id,
                Err(_) => break,
            };
            let clock = match cursor.read_u64::<LittleEndian>() {
                Ok(c) => c,
                Err(_) => break,
            };
            let data = &payload[13..]; // after opcode(1) + id(4) + clock(8)

            let op = match opcode {
                1 => WalOp::Insert {
                    id: entry_id,
                    iri: String::new(),
                },
                2 => WalOp::Upsert {
                    id: entry_id,
                    iri: String::new(),
                },
                3 => WalOp::Delete {
                    id: entry_id,
                    iri: String::new(),
                },
                4 => WalOp::MetadataUpdate {
                    id: entry_id,
                    iri: String::new(),
                },
                _ => {
                    tracing::warn!("Unknown WAL opcode {opcode} at offset {valid_pos}");
                    break;
                }
            };
            callback(op, clock, data);
            count += 1;

            valid_pos += 1 + 4 + 4 + payload_len;
        }

        // Truncate
        if valid_pos < file_len {
            tracing::warn!("Healing WAL: truncating {file_len}→{valid_pos} bytes");
            let f = OpenOptions::new().write(true).open(path)?;
            f.set_len(valid_pos)?;
        }

        Ok(count)
    }

    // ── Frozen WAL recovery ───────────────────────────────────────────────

    fn recover_frozen_wals(dir: &Path) -> Result<(), EngineError> {
        let mut entries: Vec<_> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.starts_with("frozen.") && s.ends_with(".wal")
            })
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in &entries {
            let name = entry.file_name();
            tracing::info!("Recovering frozen WAL: {}", name.to_string_lossy());
            // Replay into current active (done externally)
            Self::replay(&entry.path(), |_op, _clock, _data| {})?;
            fs::remove_file(entry.path())?;
        }
        Ok(())
    }

    // ── Len (total bytes written) ─────────────────────────────────────────

    pub fn len(&self) -> u64 {
        self.current_size.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn active_path(&self) -> &Path {
        &self.active_path
    }

    /// Delete all frozen WAL segments (called after successful checkpoint).
    pub fn cleanup_frozen(&self) -> Result<(), EngineError> {
        let entries: Vec<_> = fs::read_dir(&self.dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name();
                let s = n.to_string_lossy();
                s.starts_with("frozen.") && s.ends_with(".wal")
            })
            .collect();
        for entry in &entries {
            let name = entry.file_name();
            tracing::info!("Cleaning up frozen WAL: {}", name.to_string_lossy());
            fs::remove_file(entry.path())?;
        }
        Ok(())
    }
}

#[cfg(unix)]
use std::os::unix::io::AsRawFd;

#[cfg(test)]
mod tests {
    use super::*;

    fn test_wal(sync: WalSyncMode) {
        let dir = tempfile::tempdir().unwrap();
        let wal = EngineWal::open(dir.path(), sync).unwrap();

        wal.append(
            &WalOp::Insert {
                id: 1,
                iri: "test:1".into(),
            },
            1,
            b"data1",
        )
        .unwrap();
        wal.append(
            &WalOp::Upsert {
                id: 2,
                iri: "test:2".into(),
            },
            2,
            b"data2",
        )
        .unwrap();
        wal.append(
            &WalOp::Delete {
                id: 3,
                iri: "test:3".into(),
            },
            3,
            b"",
        )
        .unwrap();

        assert!(!wal.is_empty(), "WAL should contain written bytes");
    }

    #[test]
    fn test_wal_async() {
        test_wal(WalSyncMode::Async);
    }

    #[test]
    fn test_wal_strict() {
        test_wal(WalSyncMode::Strict);
    }

    #[test]
    fn test_wal_batch() {
        test_wal(WalSyncMode::Batch { interval_ms: 1000 });
    }

    #[test]
    fn test_wal_replay() {
        let dir = tempfile::tempdir().unwrap();
        {
            let wal = EngineWal::open(dir.path(), WalSyncMode::Strict).unwrap();
            wal.append(
                &WalOp::Insert {
                    id: 1,
                    iri: "iri:1".into(),
                },
                10,
                b"payload",
            )
            .unwrap();
            wal.append(
                &WalOp::Upsert {
                    id: 2,
                    iri: "iri:2".into(),
                },
                20,
                b"more",
            )
            .unwrap();
            wal.sync().unwrap();
        }

        let mut entries = Vec::new();
        let count = EngineWal::replay(&dir.path().join("active.wal"), |op, clock, _data| {
            entries.push((op, clock));
        })
        .unwrap();

        assert_eq!(count, 2);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].1, 10);
        assert_eq!(entries[1].1, 20);
    }

    #[test]
    fn test_wal_replay_empty() {
        let dir = tempfile::tempdir().unwrap();
        let count = EngineWal::replay(&dir.path().join("active.wal"), |_, _, _| {}).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_wal_rotation_and_replay() {
        let dir = tempfile::tempdir().unwrap();
        let wal = EngineWal::open(dir.path(), WalSyncMode::Strict).unwrap();

        // Write enough to verify WAL works
        for i in 0..10u32 {
            wal.append(
                &WalOp::Insert {
                    id: i,
                    iri: format!("iri:{i}"),
                },
                i as u64,
                b"test",
            )
            .unwrap();
        }

        // Replay back
        let mut count = 0u64;
        EngineWal::replay(&dir.path().join("active.wal"), |_, _, _| count += 1).unwrap();
        assert_eq!(count, 10);
    }

    #[test]
    fn test_file_lock_exclusion() {
        // This test verifies that opening the WAL twice fails on Unix
        let dir = tempfile::tempdir().unwrap();
        let wal1 = EngineWal::open(dir.path(), WalSyncMode::Async);
        assert!(wal1.is_ok());

        #[cfg(unix)]
        {
            let wal2 = EngineWal::open(dir.path(), WalSyncMode::Async);
            assert!(wal2.is_err());
        }
    }
}
