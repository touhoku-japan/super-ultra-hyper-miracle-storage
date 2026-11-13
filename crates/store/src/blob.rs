//! Reading and writing the plain files that hold object contents.

use std::{
    io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::join_all;
use md5::{Digest, Md5};
use tokio::{
    fs::{self, File},
    io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
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
pub struct LocalSink {
    file: File,
    tmp: PathBuf,
    final_path: PathBuf,
}

impl LocalSink {
    pub async fn create(disk: &Disk, dest: Dest) -> io::Result<Self> {
        let tmp = disk.tmp_path();
        let file = File::create_new(&tmp).await?;
        Ok(Self {
            file,
            tmp,
            final_path: dest.path(disk),
        })
    }
}

#[async_trait]
impl ReplicaSink for LocalSink {
    async fn write(&mut self, chunk: Bytes) -> io::Result<()> {
        self.file.write_all(&chunk).await
    }

    async fn commit(self: Box<Self>) -> io::Result<()> {
        let Self {
            mut file,
            tmp,
            final_path,
        } = *self;
        let result = async {
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            publish(&tmp, &final_path).await
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(&tmp).await;
        }
        result
    }

    async fn abort(self: Box<Self>) {
        drop(self.file);
        let _ = fs::remove_file(&self.tmp).await;
    }
}

struct Slot {
    disk: DiskRef,
    sink: Option<Box<dyn ReplicaSink>>,
}

impl Slot {
    async fn fail(&mut self, error: &io::Error) {
        tracing::warn!(disk = %self.disk.label(), %error, "replica write failed");
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
    mut body: R,
) -> Result<WriteOutcome> {
    let opened = join_all(targets.iter().map(|d| d.open_sink(dest))).await;
    let mut slots: Vec<Slot> = targets
        .iter()
        .zip(opened)
        .map(|(disk, sink)| Slot {
            disk: disk.clone(),
            sink: match sink {
                Ok(s) => Some(s),
                Err(error) => {
                    tracing::warn!(disk = %disk.label(), %error, "cannot open replica for writing");
                    None
                }
            },
        })
        .collect();

    let (size, md5, blake3) = match stream_into(&mut slots, required, &mut body).await {
        Ok(v) => v,
        Err(e) => {
            for slot in &mut slots {
                if let Some(sink) = slot.sink.take() {
                    sink.abort().await;
                }
            }
            return Err(e);
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
            Err(error) => tracing::warn!(disk = %disk.label(), %error, "replica commit failed"),
        }
    }

    if written.len() < required {
        for disk in &written {
            disk.remove(dest).await;
        }
        return Err(StoreError::WriteQuorum {
            required,
            written: written.len(),
        });
    }

    Ok(WriteOutcome {
        size,
        md5,
        blake3,
        written: written.iter().map(DiskRef::id).collect(),
    })
}

async fn stream_into<R: AsyncRead + Unpin>(
    slots: &mut [Slot],
    required: usize,
    body: &mut R,
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

    let mut buf = vec![0u8; CHUNK];
    let mut md5 = Md5::new();
    let mut b3 = blake3::Hasher::new();
    let mut size = 0u64;
    loop {
        let n = body.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk = Bytes::copy_from_slice(&buf[..n]);
        md5.update(&chunk);
        b3.update(&chunk);
        size += n as u64;

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
                slot.fail(&error).await;
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
    let mut file = File::open(disk.object_path(blob)).await?;
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

/// Atomically moves a fully synced temp file to its final path and syncs the directory entry.
async fn publish(tmp: &Path, final_path: &Path) -> io::Result<()> {
    let dir = final_path.parent().expect("object path has a parent");
    fs::create_dir_all(dir).await?;
    fs::rename(tmp, final_path).await?;
    sync_dir(dir).await
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

/// Concatenates parts stored on `disk` into a new object file, verifying each part against its
/// recorded BLAKE3. Returns the BLAKE3 of the whole object.
pub async fn concat_parts(
    disk: &Disk,
    upload: Uuid,
    parts: &[(Uuid, String)],
    object: Uuid,
) -> io::Result<String> {
    let tmp = disk.tmp_path();
    let result = async {
        let mut output = File::create_new(&tmp).await?;
        let mut whole = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        for (blob, expected) in parts {
            let mut input = File::open(disk.part_path(upload, *blob)).await?;
            let mut part = blake3::Hasher::new();
            loop {
                let n = input.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                part.update(&buf[..n]);
                whole.update(&buf[..n]);
                output.write_all(&buf[..n]).await?;
            }
            if part.finalize().to_hex().as_str() != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("part {blob} on this disk is corrupt"),
                ));
            }
        }
        output.flush().await?;
        output.sync_all().await?;
        drop(output);
        publish(&tmp, &disk.object_path(object)).await?;
        Ok(whole.finalize().to_hex().to_string())
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
