use super::*;
use std::io::Cursor;

fn assert_empty(path: &Path) {
    assert_eq!(fs::read_dir(path).unwrap().count(), 0);
}

#[test]
fn round_trip_streams_and_computes_sha256_in_the_specified_layout() {
    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    let content = vec![0xa5; 3 * 64 * 1024 + 17];
    let hash: [u8; 32] = Sha256::digest(&content).into();
    let blob = store
        .write(Cursor::new(&content), content.len() as u64, Some(&hash))
        .unwrap();
    assert_eq!(
        blob,
        Blob {
            hash,
            size: content.len() as u64
        }
    );
    let mut actual = Vec::new();
    store
        .open_blob(&hash)
        .unwrap()
        .read_to_end(&mut actual)
        .unwrap();
    assert_eq!(actual, content);
    let name = hex(&hash);
    assert!(
        root.path()
            .join("blobs/sha256")
            .join(&name[..2])
            .join(name)
            .is_file()
    );
    assert_eq!(fs::read(root.path().join("LAYOUT")).unwrap(), b"1\n");
    assert_empty(&root.path().join("tmp"));
    drop(store);
    let reopened = BlobStore::open(root.path()).unwrap();
    assert!(reopened.open_blob(&hash).is_ok());
}

#[test]
fn size_cap_refuses_without_temp_or_blob() {
    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    // Cross a buffer boundary so a partial upload has already reached disk.
    let content = vec![1; 64 * 1024 + 1];
    let error = store
        .write(content.as_slice(), 64 * 1024, None)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
    assert_empty(&root.path().join("tmp"));
    assert_empty(&root.path().join("blobs/sha256"));
}

#[test]
fn hash_mismatch_refuses_without_temp_or_blob() {
    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    let error = store.write(&b"abc"[..], 3, Some(&[0; 32])).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_empty(&root.path().join("tmp"));
    assert_empty(&root.path().join("blobs/sha256"));
}

#[test]
fn identical_upload_preserves_the_existing_file() {
    use std::os::unix::fs::MetadataExt;

    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    let first = store.write(&b"abc"[..], 3, None).unwrap();
    let before = store.open_blob(&first.hash).unwrap().metadata().unwrap();
    let second = store.write(&b"abc"[..], 3, None).unwrap();
    let after = store.open_blob(&second.hash).unwrap().metadata().unwrap();
    assert_eq!(first, second);
    assert_eq!(before.ino(), after.ino());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_empty(&root.path().join("tmp"));
}

#[test]
fn startup_removes_leftover_temp_files_but_keeps_blobs() {
    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    let blob = store.write(&b"abc"[..], 3, None).unwrap();
    drop(store);
    for name in ["aborted", "interrupted"] {
        fs::write(root.path().join("tmp").join(name), b"partial").unwrap();
    }
    let store = BlobStore::open(root.path()).unwrap();
    assert_empty(&root.path().join("tmp"));
    assert!(store.open_blob(&blob.hash).is_ok());
}

#[test]
fn unknown_layout_is_refused_before_cleanup() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("tmp")).unwrap();
    let leftover = root.path().join("tmp/leftover");
    fs::write(&leftover, b"partial").unwrap();
    for version in ["2\n", "", "garbage", "1\nextra"] {
        fs::write(root.path().join("LAYOUT"), version).unwrap();
        assert_eq!(
            BlobStore::open(root.path()).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(leftover.exists());
    }
}

#[test]
fn initializes_a_missing_state_directory_and_accepts_layout_without_newline() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("state");
    let store = BlobStore::open(&root).unwrap();
    assert_eq!(fs::read(root.join("LAYOUT")).unwrap(), b"1\n");
    drop(store);
    fs::write(root.join("LAYOUT"), b"1").unwrap();
    assert!(BlobStore::open(&root).is_ok());
    assert_eq!(fs::read(root.join("LAYOUT")).unwrap(), b"1");
}

#[test]
fn empty_blob_fits_zero_cap_and_missing_blob_is_not_found() {
    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    assert_eq!(
        store.open_blob(&[0; 32]).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    let blob = store.write(io::empty(), 0, None).unwrap();
    assert_eq!(blob.hash, <[u8; 32]>::from(Sha256::digest([])));
    assert_eq!(blob.size, 0);
    assert_eq!(
        store
            .open_blob(&blob.hash)
            .unwrap()
            .metadata()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        store.write(&b"a"[..], 0, None).unwrap_err().kind(),
        io::ErrorKind::FileTooLarge
    );
    assert_empty(&root.path().join("tmp"));
}

#[test]
fn read_failure_cleans_up_and_interrupted_reads_retry() {
    struct FailingReader(bool);
    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::Error::other("upload aborted"));
            }
            self.0 = true;
            buffer[0] = 1;
            Ok(1)
        }
    }
    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    assert_eq!(
        store
            .write(FailingReader(false), 10, None)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Other
    );
    assert_empty(&root.path().join("tmp"));
    assert_empty(&root.path().join("blobs/sha256"));

    struct InterruptedOnce(bool);
    impl Read for InterruptedOnce {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Ok(0);
            }
            self.0 = true;
            Err(io::Error::from(io::ErrorKind::Interrupted))
        }
    }
    assert_eq!(
        store.write(InterruptedOnce(false), 0, None).unwrap().size,
        0
    );
}

#[test]
fn publication_failure_removes_temp_file() {
    let root = tempfile::tempdir().unwrap();
    let store = BlobStore::open(root.path()).unwrap();
    let hash: [u8; 32] = Sha256::digest(b"abc").into();
    let path = store.blob_path(&hash);
    fs::write(path.parent().unwrap(), b"not a directory").unwrap();
    assert!(store.write(&b"abc"[..], 3, None).is_err());
    assert_empty(&root.path().join("tmp"));
    assert!(!path.exists());
}
