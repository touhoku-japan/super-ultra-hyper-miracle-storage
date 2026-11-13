//! Reading and writing the plain files that hold object contents.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use futures::future::join_all;
use md5::{Digest, Md5};
use tokio::{
    fs::{self, File},
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
};
use uuid::Uuid;

use crate::{
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

struct Slot {
    disk: Arc<Disk>,
    tmp: PathBuf,
    file: Option<File>,
}

impl Slot {
    async fn fail(&mut self, error: &io::Error) {
        tracing::warn!(disk = %self.disk.root.display(), %error, "replica write failed");
        self.file = None;
        let _ = fs::remove_file(&self.tmp).await;
    }
}

/// Streams `body` into a new copy on every target disk at once, without buffering the object.
///
/// A disk that fails is dropped from the write; the write as a whole fails only when fewer than
/// `required` copies survive, in which case nothing is left behind.
pub async fn write_replicas<R: AsyncRead + Unpin>(
    targets: &[Arc<Disk>],
    dest: Dest,
    required: usize,
    mut body: R,
) -> Result<WriteOutcome> {
    let mut slots = Vec::with_capacity(targets.len());
    for disk in targets {
        let tmp = disk.tmp_path();
        let file = match File::create_new(&tmp).await {
            Ok(f) => Some(f),
            Err(error) => {
                tracing::warn!(disk = %disk.root.display(), %error, "cannot create replica file");
                None
            }
        };
        slots.push(Slot {
            disk: Arc::clone(disk),
            tmp,
            file,
        });
    }

    let result = stream_into(&mut slots, required, &mut body).await;
    let (size, md5, blake3) = match result {
        Ok(v) => v,
        Err(e) => {
            discard(&mut slots).await;
            return Err(e);
        }
    };

    let mut written = Vec::new();
    for slot in &mut slots {
        let Some(mut file) = slot.file.take() else {
            continue;
        };
        let final_path = dest.path(&slot.disk);
        let persisted = async {
            file.flush().await?;
            file.sync_all().await?;
            drop(file);
            publish(&slot.tmp, &final_path).await
        }
        .await;
        match persisted {
            Ok(()) => written.push(slot.disk.id),
            Err(error) => slot.fail(&error).await,
        }
    }

    if written.len() < required {
        for slot in &slots {
            if written.contains(&slot.disk.id) {
                remove_path(&dest.path(&slot.disk)).await;
            }
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
        written,
    })
}

async fn stream_into<R: AsyncRead + Unpin>(
    slots: &mut [Slot],
    required: usize,
    body: &mut R,
) -> Result<(u64, String, String)> {
    let live = |slots: &[Slot]| slots.iter().filter(|s| s.file.is_some()).count();
    let check = |slots: &[Slot]| {
        let written = live(slots);
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
        let chunk = &buf[..n];
        md5.update(chunk);
        b3.update(chunk);
        size += n as u64;

        let results = join_all(slots.iter_mut().map(|s| async move {
            match s.file.as_mut() {
                Some(f) => f.write_all(chunk).await,
                None => Ok(()),
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

async fn discard(slots: &mut [Slot]) {
    for slot in slots {
        slot.file = None;
        let _ = fs::remove_file(&slot.tmp).await;
    }
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
    /// The source file could not be opened; nothing was written.
    SourceUnreadable,
}

/// Copies a blob between disks, verifying the contents against `expected_blake3` before the
/// copy becomes visible.
pub async fn copy_replica(
    src: &Disk,
    dst: &Disk,
    blob: Uuid,
    expected_blake3: &str,
) -> io::Result<CopyOutcome> {
    let mut input = match File::open(src.object_path(blob)).await {
        Ok(f) => f,
        Err(error) => {
            tracing::warn!(disk = %src.root.display(), %blob, %error, "cannot open source replica");
            return Ok(CopyOutcome::SourceUnreadable);
        }
    };
    let tmp = dst.tmp_path();
    let result = async {
        let mut output = File::create_new(&tmp).await?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = input.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            output.write_all(&buf[..n]).await?;
        }
        if hasher.finalize().to_hex().as_str() != expected_blake3 {
            return Ok(CopyOutcome::SourceCorrupt);
        }
        output.flush().await?;
        output.sync_all().await?;
        drop(output);
        publish(&tmp, &dst.object_path(blob)).await?;
        Ok(CopyOutcome::Copied)
    }
    .await;
    if !matches!(result, Ok(CopyOutcome::Copied)) {
        let _ = fs::remove_file(&tmp).await;
    }
    result
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
