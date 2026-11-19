//! Reading and writing the plain files that hold object contents.

use std::{
    io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::future::join_all;
use md5::{Digest, Md5};
use tokio::{
    fs::{self, File},
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt},
};
use uuid::Uuid;

use crate::{
    cluster::{BoxReader, DiskRef, ReplicaSink},
    disk::{Disk, DiskId},
    error::{Result, StoreError},
};

const CHUNK: usize = 1 << 20;

/// Where a write lands on each target disk.
#[derive(Debug, Clone, Copy)]
pub enum Dest {
    Object(Uuid),
    Part { upload: Uuid, blob: Uuid },
}

impl Dest {
    pub fn path(self, disk: &Disk) -> PathBuf {
        match self {
            Self::Object(blob) => disk.object_path(blob),
            Self::Part { upload, blob } => disk.part_path(upload, blob),
        }
    }
}

#[derive(Debug)]
pub struct WriteOutcome {
    pub size: u64,
    /// Hex MD5 of the contents (the S3 ETag of a single-part upload).
    pub md5: String,
    /// Hex BLAKE3 of the contents, used to detect corrupted copies.
    pub blake3: String,
    /// Disks that durably hold a copy.
    pub written: Vec<DiskId>,
}

/// A write to a temp file on a local disk, published under its final name on commit.
///
/// Writes go through a blocking task with a std file rather than `tokio::fs::File`, which
/// copies every buffer into one of its own first.
pub struct LocalSink {
    file: Option<std::fs::File>,
    tmp: PathBuf,
    final_path: PathBuf,
}

impl LocalSink {
    pub async fn create(disk: &Disk, dest: Dest) -> io::Result<Self> {
        let tmp = disk.tmp_path();
        let file = File::create_new(&tmp).await?.into_std().await;
        Ok(Self {
            file: Some(file),
            tmp,
            final_path: dest.path(disk),
        })
    }

    /// Runs `f` on the file in a blocking task.
    async fn with_file<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut std::fs::File) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| io::Error::other("an earlier write to this file was interrupted"))?;
        let (file, result) = tokio::task::spawn_blocking(move || {
            let r = f(&mut file);
            (file, r)
        })
        .await
        .map_err(io::Error::other)?;
        self.file = Some(file);
        result
    }
}

#[async_trait]
impl ReplicaSink for LocalSink {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        self.with_file(move |f| std::io::Write::write_all(f, &chunk))
            .await
    }

    async fn commit(mut self: Box<Self>) -> io::Result<()> {
        let result = async {
            self.with_file(|f| f.sync_all()).await?;
            drop(self.file.take());
            publish(&self.tmp, &self.final_path).await
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&self.tmp).await;
        }
        result
    }

    async fn abort(mut self: Box<Self>) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.tmp).await;
    }
}

struct Slot {
    disk: DiskRef,
    sink: Option<Box<dyn ReplicaSink>>,
}

impl Slot {
    /// Drops the copy; records its disk in `full` when it ran out of space.
    async fn fail(&mut self, error: &io::Error, full: &mut Vec<DiskId>) {
        tracing::warn!(disk = %self.disk.label(), %error, "replica write failed");
        if error.kind() == io::ErrorKind::StorageFull {
            full.push(self.disk.id());
        }
        if let Some(sink) = self.sink.take() {
            sink.abort().await;
        }
    }
}

/// Streams `body` into a new copy on every target disk at once (local or remote), without
/// buffering the object.
///
/// A disk that fails is dropped from the write; the write as a whole fails only when fewer than
/// `required` copies survive, in which case nothing is left behind.
pub async fn write_replicas<R: AsyncRead + Unpin>(
    targets: &[DiskRef],
    dest: Dest,
    required: usize,
    body: R,
) -> Result<WriteOutcome> {
    Replicas::open(targets, dest)
        .await
        .write(required, body)
        .await
}

/// Copies being written, opened before any of the body is read so that a disk that cannot take
/// a copy (a node that died since the last ping, a full or failed disk) can still be replaced.
pub struct Replicas {
    dest: Dest,
    slots: Vec<Slot>,
    tried: Vec<DiskId>,
}

impl Replicas {
    pub async fn open(targets: &[DiskRef], dest: Dest) -> Self {
        let mut r = Self {
            dest,
            slots: Vec::new(),
            tried: Vec::new(),
        };
        r.add(targets).await;
        r
    }

    /// Opens copies on more disks; those that fail to open are only remembered in [`Self::tried`].
    pub async fn add(&mut self, targets: &[DiskRef]) {
        let dest = self.dest;
        let opened = join_all(targets.iter().map(|d| d.open_sink(dest))).await;
        for (disk, sink) in targets.iter().zip(opened) {
            self.tried.push(disk.id());
            match sink {
                Ok(sink) => self.slots.push(Slot {
                    disk: disk.clone(),
                    sink: Some(sink),
                }),
                Err(error) => {
                    tracing::warn!(disk = %disk.label(), %error, "cannot open replica for writing");
                }
            }
        }
    }

    /// The disks with an open copy.
    pub fn open_targets(&self) -> Vec<DiskRef> {
        self.slots.iter().map(|s| s.disk.clone()).collect()
    }

    /// Disks with an open copy.
    pub fn open_disks(&self) -> Vec<DiskId> {
        self.slots.iter().map(|s| s.disk.id()).collect()
    }

    /// Every disk tried so far, opened or not.
    pub fn tried(&self) -> &[DiskId] {
        &self.tried
    }

    /// Discards the open copies.
    pub async fn abort(self) {
        join_all(
            self.slots
                .into_iter()
                .filter_map(|s| s.sink)
                .map(|sink| sink.abort()),
        )
        .await;
    }

    /// Streams `body` into the open copies and commits them.
    pub async fn write<R: AsyncRead + Unpin>(
        self,
        required: usize,
        body: R,
    ) -> Result<WriteOutcome> {
        self.write_tracking(required, body).await.0
    }

    /// Like [`Replicas::write`]; also returns the disks that ran out of space during the write.
    /// When those cost the write its quorum, the error is [`StoreError::InsufficientStorage`].
    pub async fn write_tracking<R: AsyncRead + Unpin>(
        self,
        required: usize,
        mut body: R,
    ) -> (Result<WriteOutcome>, Vec<DiskId>) {
        let Self {
            dest, mut slots, ..
        } = self;
        let mut full = Vec::new();
        let (size, md5, blake3) =
            match stream_into(&mut slots, required, &mut body, &mut full).await {
                Ok(v) => v,
                Err(e) => {
                    for slot in &mut slots {
                        if let Some(sink) = slot.sink.take() {
                            sink.abort().await;
                        }
                    }
                    return (Err(out_of_space(e, &full)), full);
                }
            };

        let committing: Vec<(DiskRef, Box<dyn ReplicaSink>)> = slots
            .into_iter()
            .filter_map(|s| s.sink.map(|sink| (s.disk, sink)))
            .collect();
        let results = join_all(committing.into_iter().map(|(disk, sink)| async move {
            let r = sink.commit().await;
            (disk, r)
        }))
        .await;
        let mut written = Vec::new();
        for (disk, r) in results {
            match r {
                Ok(()) => written.push(disk),
                Err(error) => {
                    tracing::warn!(disk = %disk.label(), %error, "replica commit failed");
                    if error.kind() == io::ErrorKind::StorageFull {
                        full.push(disk.id());
                    }
                }
            }
        }

        if written.len() < required {
            for disk in &written {
                disk.remove(dest).await;
            }
            let e = StoreError::WriteQuorum {
                required,
                written: written.len(),
            };
            return (Err(out_of_space(e, &full)), full);
        }

        let outcome = WriteOutcome {
            size,
            md5,
            blake3,
            written: written.iter().map(DiskRef::id).collect(),
        };
        (Ok(outcome), full)
    }
}

/// A lost quorum caused by disks running out of space is reported as such.
fn out_of_space(e: StoreError, full: &[DiskId]) -> StoreError {
    match e {
        StoreError::WriteQuorum { required, written } if !full.is_empty() => {
            StoreError::InsufficientStorage {
                required,
                available: written,
            }
        }
        e => e,
    }
}

async fn stream_into<R: AsyncRead + Unpin>(
    slots: &mut [Slot],
    required: usize,
    body: &mut R,
    full: &mut Vec<DiskId>,
) -> Result<(u64, String, String)> {
    let check = |slots: &[Slot]| {
        let written = slots.iter().filter(|s| s.sink.is_some()).count();
        if written < required {
            Err(StoreError::WriteQuorum { required, written })
        } else {
            Ok(())
        }
    };
    check(slots)?;

    let mut md5 = Md5::new();
    let mut b3 = blake3::Hasher::new();
    let mut size = 0u64;
    let mut eof = false;
    while !eof {
        // Gather a full chunk before passing it on: request bodies arrive in pieces of a few
        // KiB, and each chunk costs a write per copy (a channel send and an HTTP frame for a
        // remote one). The chunk is read straight into the buffer it is sent in, not copied.
        let mut buf = BytesMut::with_capacity(CHUNK);
        while buf.len() < CHUNK {
            if body.read_buf(&mut buf).await? == 0 {
                eof = true;
                break;
            }
        }
        if buf.is_empty() {
            break;
        }
        let chunk = buf.freeze();
        md5.update(&chunk);
        b3.update(&chunk);
        size += chunk.len() as u64;

        let results = join_all(slots.iter_mut().map(|s| {
            let chunk = chunk.clone();
            async move {
                match s.sink.as_mut() {
                    Some(sink) => sink.write(chunk).await,
                    None => Ok(()),
                }
            }
        }))
        .await;
        for (slot, res) in slots.iter_mut().zip(results) {
            if let Err(error) = res {
                slot.fail(&error, full).await;
            }
        }
        check(slots)?;
    }
    Ok((
        size,
        hex::encode(md5.finalize()),
        b3.finalize().to_hex().to_string(),
    ))
}

/// Opens `range` of a local object file, checking that the file has the expected size.
pub async fn read_local(
    disk: &Disk,
    blob: Uuid,
    size: u64,
    range: std::ops::Range<u64>,
) -> io::Result<BoxReader> {
    read_local_path(&disk.object_path(blob), size, range).await
}

/// Opens `range` of a local file (an object or a part), checking that it has the expected size.
pub async fn read_local_path(
    path: &Path,
    size: u64,
    range: std::ops::Range<u64>,
) -> io::Result<BoxReader> {
    let mut file = File::open(path).await?;
    let len = file.metadata().await?.len();
    if len != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("replica has {len} bytes, expected {size}"),
        ));
    }
    if range.start > range.end || range.end > size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "range out of bounds",
        ));
    }
    file.seek(io::SeekFrom::Start(range.start)).await?;
    Ok(Box::new(file.take(range.end - range.start)))
}

/// Hashes everything read through it and checks it against `expected` (hex BLAKE3) before
/// handing out the last bytes of the object: on a mismatch those bytes are withheld and the
/// read fails with [`io::ErrorKind::InvalidData`]. A consumer therefore never receives the
/// whole `size` bytes of a corrupt copy: an HTTP response stays short of its Content-Length
/// (an HTTP server stops reading the body once Content-Length bytes are out, so checking only
/// at end of file would be too late), and a copy into a new object is discarded.
/// `on_mismatch` runs once, when the mismatch is found.
pub struct VerifyingReader {
    inner: BoxReader,
    hasher: blake3::Hasher,
    size: u64,
    read: u64,
    expected: String,
    on_mismatch: Option<Box<dyn FnOnce() + Send + Sync>>,
    state: VerifyState,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VerifyState {
    Reading,
    Verified,
    Corrupt,
}

impl VerifyingReader {
    /// `inner` must yield the whole object, `size` bytes.
    pub fn new(
        inner: BoxReader,
        size: u64,
        expected: String,
        on_mismatch: Box<dyn FnOnce() + Send + Sync>,
    ) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            size,
            read: 0,
            expected,
            on_mismatch: Some(on_mismatch),
            state: VerifyState::Reading,
        }
    }

    fn corrupt() -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "object copy does not match its BLAKE3 hash",
        )
    }

    /// Checks the hash once all bytes went through it; false on a mismatch.
    fn verify(&mut self) -> bool {
        if self.read == self.size && self.hasher.finalize().to_hex().as_str() == self.expected {
            self.state = VerifyState::Verified;
            return true;
        }
        self.state = VerifyState::Corrupt;
        if let Some(f) = self.on_mismatch.take() {
            f();
        }
        false
    }
}

impl AsyncRead for VerifyingReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;
        let this = self.get_mut();
        match this.state {
            VerifyState::Verified => return Poll::Ready(Ok(())),
            VerifyState::Corrupt => return Poll::Ready(Err(Self::corrupt())),
            VerifyState::Reading => {}
        }
        let before = buf.filled().len();
        std::task::ready!(std::pin::Pin::new(&mut this.inner).poll_read(cx, buf))?;
        let chunk = &buf.filled()[before..];
        this.hasher.update(chunk);
        this.read += chunk.len() as u64;
        // Decide at the last byte (or at a premature end of file) before returning anything.
        if (this.read >= this.size || chunk.is_empty()) && !this.verify() {
            buf.set_filled(before);
            return Poll::Ready(Err(Self::corrupt()));
        }
        Poll::Ready(Ok(()))
    }
}

/// Atomically moves a fully synced temp file to its final path and syncs the directory entry.
///
/// Creates at most the two fan-out levels below the destination's base directory (`objects/`,
/// or the upload's directory), never the base itself: if the disk was unmounted or removed,
/// recreating its root would put the file on the wrong filesystem.
async fn publish(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let dir = final_path.parent().expect("object path has a parent");
    create_below_existing(dir, 2).await?;
    fs::rename(tmp, final_path).await?;
    sync_dir(dir).await
}

/// Creates `dir` and up to `levels - 1` missing parents; fails when more would be needed.
async fn create_below_existing(dir: &Path, levels: usize) -> io::Result<()> {
    match fs::metadata(dir).await {
        Ok(_) => return Ok(()),
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        Err(e) if levels == 0 => return Err(e),
        Err(_) => {}
    }
    if let Some(parent) = dir.parent() {
        Box::pin(create_below_existing(parent, levels - 1)).await?;
    }
    match fs::create_dir(dir).await {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other,
    }
}

async fn sync_dir(dir: &Path) -> io::Result<()> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || std::fs::File::open(dir)?.sync_all())
        .await
        .expect("sync task panicked")
}

/// Size and hex BLAKE3 of a file.
pub async fn hash_file(path: &Path) -> io::Result<(u64, String)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut hasher = blake3::Hasher::new();
        let file = std::fs::File::open(path)?;
        hasher.update_reader(file)?;
        Ok((hasher.count(), hasher.finalize().to_hex().to_string()))
    })
    .await
    .expect("hash task panicked")
}

#[derive(Debug, PartialEq, Eq)]
pub enum CopyOutcome {
    Copied,
    /// The source's contents did not match the expected hash; nothing was written.
    SourceCorrupt,
    /// The source could not be read; nothing was written.
    SourceUnreadable,
}

/// Copies an object file between disks (on any nodes), verifying the contents against
/// `expected_blake3` before the copy becomes visible.
pub async fn copy_replica(
    src: &DiskRef,
    dst: &DiskRef,
    blob: Uuid,
    size: u64,
    expected_blake3: &str,
) -> io::Result<CopyOutcome> {
    let mut input = match src.read(blob, size, 0..size).await {
        Ok(r) => r,
        Err(error) => {
            tracing::warn!(disk = %src.label(), %blob, %error, "cannot read source replica");
            return Ok(CopyOutcome::SourceUnreadable);
        }
    };
    let mut sink = dst.open_sink(Dest::Object(blob)).await?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match input.read(&mut buf).await {
            Ok(n) => n,
            Err(error) => {
                tracing::warn!(disk = %src.label(), %blob, %error, "source replica read failed");
                sink.abort().await;
                return Ok(CopyOutcome::SourceUnreadable);
            }
        };
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if let Err(e) = sink.write(Bytes::copy_from_slice(&buf[..n])).await {
            sink.abort().await;
            return Err(e);
        }
    }
    if hasher.finalize().to_hex().as_str() != expected_blake3 {
        sink.abort().await;
        return Ok(CopyOutcome::SourceCorrupt);
    }
    sink.commit().await?;
    Ok(CopyOutcome::Copied)
}

/// A part to concatenate: its file, its BLAKE3 and, when the upload recorded one, a CRC of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinPart {
    pub blob: Uuid,
    pub blake3: String,
    pub crc: Option<PartCrc>,
}

/// A CRC of a part, as recorded when it was uploaded (its S3 checksum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartCrc {
    pub algorithm: CrcKind,
    pub value: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CrcKind {
    Crc32,
    Crc32c,
    Crc64Nvme,
}

impl CrcKind {
    fn algorithm(self) -> crc_fast::CrcAlgorithm {
        match self {
            Self::Crc32 => crc_fast::CrcAlgorithm::Crc32IsoHdlc,
            Self::Crc32c => crc_fast::CrcAlgorithm::Crc32Iscsi,
            Self::Crc64Nvme => crc_fast::CrcAlgorithm::Crc64Nvme,
        }
    }
}

/// Checks one part while it is copied into the object.
enum PartCheck {
    Blake3(Box<blake3::Hasher>, String),
    Crc(Box<crc_fast::Digest>, u64),
}

impl PartCheck {
    fn new(part: &JoinPart) -> Self {
        match part.crc {
            Some(c) => Self::Crc(
                Box::new(crc_fast::Digest::new(c.algorithm.algorithm())),
                c.value,
            ),
            None => Self::Blake3(Box::new(blake3::Hasher::new()), part.blake3.clone()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            Self::Blake3(h, _) => {
                h.update(data);
            }
            Self::Crc(d, _) => d.update(data),
        }
    }

    fn matches(&self) -> bool {
        match self {
            Self::Blake3(h, expected) => h.finalize().to_hex().as_str() == expected,
            Self::Crc(d, expected) => d.finalize() == *expected,
        }
    }
}

/// Concatenates parts stored on `disk` into a new object file, verifying each part against its
/// recorded BLAKE3. Returns the BLAKE3 of the whole object.
pub async fn concat_parts(
    disk: &Disk,
    upload: Uuid,
    parts: &[JoinPart],
    object: Uuid,
) -> io::Result<String> {
    let tmp = disk.tmp_path();
    let inputs: Vec<(PathBuf, JoinPart)> = parts
        .iter()
        .map(|p| (disk.part_path(upload, p.blob), p.clone()))
        .collect();
    let result = async {
        // One blocking task with std files: no copies through tokio's file buffers.
        let out = tmp.clone();
        let whole = tokio::task::spawn_blocking(move || -> io::Result<String> {
            use std::io::{Read, Write};
            let mut output = std::fs::File::create_new(&out)?;
            let mut whole = blake3::Hasher::new();
            let mut buf = vec![0u8; CHUNK];
            for (path, part) in inputs {
                let mut input = std::fs::File::open(path)?;
                // A part's CRC, when there is one, is far cheaper to check than its BLAKE3 and
                // as good at catching a damaged file; the object's BLAKE3 is computed anyway.
                let mut check = PartCheck::new(&part);
                loop {
                    let n = input.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    check.update(&buf[..n]);
                    whole.update(&buf[..n]);
                    output.write_all(&buf[..n])?;
                }
                if !check.matches() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("part {} on this disk is corrupt", part.blob),
                    ));
                }
            }
            output.sync_all()?;
            Ok(whole.finalize().to_hex().to_string())
        })
        .await
        .map_err(io::Error::other)??;
        publish(&tmp, &disk.object_path(object)).await?;
        Ok(whole)
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&tmp).await;
    }
    result
}

/// Deletes all part files of an upload from a disk.
pub async fn remove_upload_dir(disk: &Disk, upload: Uuid) {
    match fs::remove_dir_all(disk.upload_dir(upload)).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(disk = %disk.root.display(), %upload, %error, "failed to remove upload parts");
        }
    }
}

/// Deletes a file; missing files are fine, other errors are logged.
pub async fn remove_path(path: &Path) {
    match fs::remove_file(path).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to remove file");
        }
    }
}

/// Deletes a blob's file from a disk. Missing files are fine; other errors are logged, since the
/// orphaned file only wastes space.
pub async fn remove(disk: &Disk, blob: Uuid) {
    match fs::remove_file(disk.object_path(blob)).await {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(disk = %disk.root.display(), %blob, %error, "failed to remove replica file");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    async fn disk_with_parts(parts: &[&[u8]]) -> (tempfile::TempDir, Disk, Uuid, Vec<Uuid>) {
        let dir = tempfile::tempdir().unwrap();
        let disk = Disk::open(dir.path(), "node1").await.unwrap();
        let upload = Uuid::new_v4();
        let mut blobs = Vec::new();
        for data in parts {
            let blob = Uuid::new_v4();
            write_replicas(
                &[DiskRef::Local(Arc::new(
                    Disk::open(dir.path(), "node1").await.unwrap(),
                ))],
                Dest::Part { upload, blob },
                1,
                *data,
            )
            .await
            .unwrap();
            blobs.push(blob);
        }
        (dir, disk, upload, blobs)
    }

    fn crc64(data: &[u8]) -> PartCrc {
        let mut d = crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc64Nvme);
        d.update(data);
        PartCrc {
            algorithm: CrcKind::Crc64Nvme,
            value: d.finalize(),
        }
    }

    fn b3(data: &[u8]) -> String {
        blake3::hash(data).to_hex().to_string()
    }

    #[tokio::test]
    async fn parts_are_checked_by_crc_when_there_is_one_and_by_blake3_otherwise() {
        let (a, b): (&[u8], &[u8]) = (b"first part", b"second part");
        let (_dir, disk, upload, blobs) = disk_with_parts(&[a, b]).await;
        let parts = vec![
            // With a CRC, the BLAKE3 is not what is checked.
            JoinPart {
                blob: blobs[0],
                blake3: "not checked".into(),
                crc: Some(crc64(a)),
            },
            JoinPart {
                blob: blobs[1],
                blake3: b3(b),
                crc: None,
            },
        ];
        let object = Uuid::new_v4();
        let hash = concat_parts(&disk, upload, &parts, object).await.unwrap();
        assert_eq!(hash, b3(b"first partsecond part"));
        assert_eq!(
            std::fs::read(disk.object_path(object)).unwrap(),
            b"first partsecond part"
        );
    }

    #[tokio::test]
    async fn a_part_that_fails_its_check_fails_the_assembly() {
        let data: &[u8] = b"some part";
        let (_dir, disk, upload, blobs) = disk_with_parts(&[data]).await;
        std::fs::write(disk.part_path(upload, blobs[0]), b"some pXrt").unwrap();
        for crc in [Some(crc64(data)), None] {
            let parts = [JoinPart {
                blob: blobs[0],
                blake3: b3(data),
                crc,
            }];
            let object = Uuid::new_v4();
            let err = concat_parts(&disk, upload, &parts, object)
                .await
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{crc:?}");
            assert!(!disk.object_path(object).exists());
        }
    }
}
