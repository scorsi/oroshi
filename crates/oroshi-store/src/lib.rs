#![forbid(unsafe_code)]

mod database;

pub use database::{Database, DatabaseError, Reader};

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};

/// An immutable blob's identity and size in bytes, ready for a metadata commit.
#[derive(Debug, PartialEq, Eq)]
pub struct Blob {
    pub hash: [u8; 32],
    pub size: u64,
}

/// One serving instance's blob store on a local, durable filesystem (ADR 0014).
pub struct BlobStore {
    root: PathBuf,
    publish: Mutex<()>,
}

impl BlobStore {
    /// Initialize layout 1 and remove leftover uploads before accepting writes.
    /// Open only once at startup: cleanup must not race active uploads.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let layout = root.join("LAYOUT");
        let initialize = match fs::read(&layout) {
            Ok(version) if version == b"1" || version == b"1\n" => false,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown LAYOUT version",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(error),
        };
        create_dir_all_durable(root, &mut sync_directory)?;
        sync_directory(root)?;
        for directory in [
            root.join("tmp"),
            root.join("blobs"),
            root.join("blobs/sha256"),
        ] {
            create_dir_all_durable(&directory, &mut sync_directory)?;
            sync_directory(&directory)?;
        }
        for entry in fs::read_dir(root.join("tmp"))? {
            let entry = entry?;
            // Uploads only create files; a stray directory must not turn startup into a restart loop.
            if !entry.file_type()?.is_dir() {
                fs::remove_file(entry.path())?;
            }
        }
        File::open(root.join("tmp"))?.sync_all()?;
        if initialize {
            let mut temp = TempFile::create(&root.join("tmp"))?;
            temp.file.write_all(b"1\n")?;
            temp.file.sync_all()?;
            fs::rename(&temp.path, layout)?;
            File::open(root)?.sync_all()?;
        }
        Ok(Self {
            root: root.to_owned(),
            publish: Mutex::new(()),
        })
    }

    /// Stream at most `max_bytes` to disk, validating before durable publication.
    /// Size refusal is `FileTooLarge`; an expected-hash mismatch is `InvalidData`.
    pub fn write(
        &self,
        mut reader: impl Read,
        max_bytes: u64,
        expected_hash: Option<&[u8; 32]>,
    ) -> io::Result<Blob> {
        let mut temp = TempFile::create(&self.root.join("tmp"))?;
        let mut digest = Sha256::new();
        let mut size = 0;
        let mut buffer = [0; 64 * 1024];
        loop {
            let length = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(length) => length,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if length as u64 > max_bytes - size {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    "blob exceeds size cap",
                ));
            }
            temp.file.write_all(&buffer[..length])?;
            digest.update(&buffer[..length]);
            size += length as u64;
        }
        let hash = digest.finalize().into();
        if expected_hash.is_some_and(|expected| expected != &hash) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "blob hash mismatch",
            ));
        }
        temp.file.sync_all()?;
        let path = self.blob_path(&hash);
        let directory = path.parent().unwrap();
        // Hashed outside the lock: a blob whose bytes no longer match its name is replaced.
        let sound = path.try_exists()? && hash_file(&path)? == hash;
        // Serialize the existence check and rename so concurrent uploads never replace a blob.
        let _publish = self
            .publish
            .lock()
            .map_err(|_| io::Error::other("blob publication lock poisoned"))?;
        fs::create_dir_all(directory)?;
        File::open(directory.parent().unwrap())?.sync_all()?;
        if !sound || !path.try_exists()? {
            fs::rename(&temp.path, &path)?;
        }
        File::open(directory)?.sync_all()?;
        Ok(Blob { hash, size })
    }

    /// Open a blob without buffering its contents; absent blobs return `NotFound`.
    pub fn open_blob(&self, hash: &[u8; 32]) -> io::Result<File> {
        File::open(self.blob_path(hash))
    }

    fn blob_path(&self, hash: &[u8; 32]) -> PathBuf {
        let name = hex(hash);
        self.root.join("blobs/sha256").join(&name[..2]).join(name)
    }
}

fn hash_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    Ok(digest.finalize().into())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[(byte >> 4) as usize] as char,
                DIGITS[(byte & 15) as usize] as char,
            ]
        })
        .collect()
}

struct TempFile {
    path: PathBuf,
    file: File,
}

impl TempFile {
    fn create(directory: &Path) -> io::Result<Self> {
        let mut random = File::open("/dev/urandom")?;
        loop {
            let mut bytes = [0; 16];
            random.read_exact(&mut bytes)?;
            let path = directory.join(hex(&bytes));
            match File::create_new(&path) {
                Ok(file) => return Ok(Self { path, file }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // A crash or failed cleanup is recovered by startup's temp sweep.
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests;

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

// Persist each new ancestor before creating entries beneath it.
fn create_dir_all_durable(
    path: &Path,
    sync: &mut impl FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !path.is_dir() {
        create_dir_all_durable(parent, sync)?;
        fs::create_dir_all(path)?;
    }
    sync(parent)
}
