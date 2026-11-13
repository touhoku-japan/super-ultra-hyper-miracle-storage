//! A disk is a directory (normally a mount point) on an ordinary filesystem.
//!
//! Layout:
//! ```text
//! <root>/.suhms/format.json      disk identity (stable across remounts/reordering)
//! <root>/.suhms/tmp/<uuid>        in-progress writes, cleared on startup
//! <root>/.suhms/multipart/<upload>/<blob-id>  parts of an in-progress multipart upload
//! <root>/objects/ab/cd/<blob-id>  object contents, stored verbatim
//! ```

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Result, StoreError};

pub type DiskId = Uuid;

const META_DIR: &str = ".suhms";
const FORMAT_FILE: &str = "format.json";
const TMP_DIR: &str = "tmp";
const OBJECTS_DIR: &str = "objects";
const MULTIPART_DIR: &str = "multipart";
const FORMAT_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct FormatFile {
    version: u32,
    disk_id: Uuid,
}

#[derive(Debug)]
pub struct Disk {
    pub id: DiskId,
    pub node_id: String,
    pub root: PathBuf,
}

impl Disk {
    /// Opens a disk, formatting it on first use.
    ///
    /// The root must already exist: creating it automatically would silently write into the
    /// parent filesystem whenever a mount is missing.
    pub async fn open(root: &Path, node_id: &str) -> Result<Self> {
        let root = root.to_path_buf();
        let node_id = node_id.to_owned();
        tokio::task::spawn_blocking(move || Self::open_blocking(root, node_id))
            .await
            .expect("disk open task panicked")
    }

    fn open_blocking(root: PathBuf, node_id: String) -> Result<Self> {
        let unavailable = |source| StoreError::DiskUnavailable {
            path: root.clone(),
            source,
        };
        let md = fs::metadata(&root).map_err(unavailable)?;
        if !md.is_dir() {
            return Err(StoreError::DiskFormat {
                path: root,
                reason: "not a directory".into(),
            });
        }

        let meta_dir = root.join(META_DIR);
        let format_path = meta_dir.join(FORMAT_FILE);
        let id = match fs::read(&format_path) {
            Ok(bytes) => {
                let format: FormatFile =
                    serde_json::from_slice(&bytes).map_err(|e| StoreError::DiskFormat {
                        path: root.clone(),
                        reason: format!("unreadable {FORMAT_FILE}: {e}"),
                    })?;
                if format.version != FORMAT_VERSION {
                    return Err(StoreError::DiskFormat {
                        path: root,
                        reason: format!("unsupported format version {}", format.version),
                    });
                }
                format.disk_id
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::format(&root)?,
            Err(e) => return Err(unavailable(e)),
        };

        fs::create_dir_all(root.join(OBJECTS_DIR)).map_err(unavailable)?;
        fs::create_dir_all(meta_dir.join(TMP_DIR)).map_err(unavailable)?;

        Ok(Self { id, node_id, root })
    }

    /// Writes a new identity to an empty directory. Refuses non-empty directories so a typo in
    /// the config cannot claim a directory that holds unrelated data.
    fn format(root: &Path) -> Result<DiskId> {
        let unavailable = |source| StoreError::DiskUnavailable {
            path: root.to_path_buf(),
            source,
        };
        let foreign = fs::read_dir(root)
            .map_err(unavailable)?
            .filter_map(std::result::Result::ok)
            .any(|e| e.file_name() != "lost+found");
        if foreign {
            return Err(StoreError::DiskFormat {
                path: root.to_path_buf(),
                reason: "directory is not empty and has no disk identity; refusing to format"
                    .into(),
            });
        }

        let meta_dir = root.join(META_DIR);
        fs::create_dir_all(&meta_dir).map_err(unavailable)?;
        let id = Uuid::new_v4();
        let body = serde_json::to_vec_pretty(&FormatFile {
            version: FORMAT_VERSION,
            disk_id: id,
        })
        .expect("format file serializes");
        let tmp = meta_dir.join(format!("{FORMAT_FILE}.tmp"));
        fs::write(&tmp, body).map_err(unavailable)?;
        fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(unavailable)?;
        fs::rename(&tmp, meta_dir.join(FORMAT_FILE)).map_err(unavailable)?;
        fs::File::open(&meta_dir)
            .and_then(|f| f.sync_all())
            .map_err(unavailable)?;
        tracing::info!(path = %root.display(), disk_id = %id, "formatted new disk");
        Ok(id)
    }

    /// Deletes leftovers of writes interrupted by a crash (their objects were never committed).
    /// Only safe while nothing else is writing to this disk, i.e. at server startup.
    pub async fn clear_tmp(&self) -> io::Result<()> {
        let tmp = self.root.join(META_DIR).join(TMP_DIR);
        tokio::task::spawn_blocking(move || {
            if tmp.exists() {
                fs::remove_dir_all(&tmp)?;
            }
            fs::create_dir_all(&tmp)
        })
        .await
        .expect("cleanup task panicked")
    }

    /// Final location of a blob's file on this disk.
    pub fn object_path(&self, blob: Uuid) -> PathBuf {
        let name = blob.simple().to_string();
        self.root
            .join(OBJECTS_DIR)
            .join(&name[0..2])
            .join(&name[2..4])
            .join(name)
    }

    /// Directory holding the parts of one multipart upload on this disk.
    pub fn upload_dir(&self, upload: Uuid) -> PathBuf {
        self.root
            .join(META_DIR)
            .join(MULTIPART_DIR)
            .join(upload.simple().to_string())
    }

    /// Location of one uploaded part's file.
    pub fn part_path(&self, upload: Uuid, blob: Uuid) -> PathBuf {
        self.upload_dir(upload).join(blob.simple().to_string())
    }

    /// A fresh temporary path for an in-progress write.
    pub fn tmp_path(&self) -> PathBuf {
        self.root
            .join(META_DIR)
            .join(TMP_DIR)
            .join(Uuid::new_v4().simple().to_string())
    }

    /// Bytes available to unprivileged writers; 0 when the filesystem cannot be queried.
    pub fn available_space(&self) -> u64 {
        fs4::available_space(&self.root).unwrap_or(0)
    }
}
