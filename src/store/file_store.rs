use crate::store::*;
use blake2::{Blake2b, Digest};
use chrono::prelude::*;
use failure::{Error, Fail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a file must persist on disk after an access
const PERSISTENCY_DURATION: Duration = Duration::from_secs(600);
/// Whether to check the file integrity on the store before getting it
const CHECK_INTEGRITY: bool = true;

/// The type of an hash of a file
type HashData = Vec<u8>;

/// This will manage a file storage directory with the ability of:
/// * remove files not needed anymore that takes too much space
/// * locking so no other instances of FileStorage can access the storage while
///   this is still running
/// * do not remove files useful for the current computations
#[derive(Debug)]
pub struct FileStore {
    /// Base directory of the FileStore
    base_path: PathBuf,
    /// Handle of the file with the data of the store. This handle keeps the
    /// lock alive.
    file: File,
    /// Data of the FileStore with the list of known files
    data: FileStoreData,
}

/// Handle of a file in the FileStore, this must be computables given the
/// content of the file, i.e. an hash of the content.
#[derive(Clone, Serialize, Deserialize)]
pub struct FileStoreKey {
    /// An hash of the content of the file
    hash: HashData,
}

/// Errors generated by the FileStore
#[derive(Debug, Fail)]
pub enum FileStoreError {
    #[fail(display = "invalid path provided")]
    InvalidPath,
    #[fail(display = "file not present in the store")]
    NotFound,
}

/// The content of an entry of a file in the FileStore
#[derive(Debug, Serialize, Deserialize)]
struct FileStoreItem {
    /// Timestamp of when the file may be deleted
    persistent: DateTime<Utc>,
    // TODO change this to a refcounted struct which holds the lock to that
    // file
}

/// Internal data of the FileStore
#[derive(Debug, Serialize, Deserialize)]
struct FileStoreData {
    /// List of the known files, this should be JSON serializable
    items: HashMap<String, FileStoreItem>,
}

impl FileStoreKey {
    /// Make the key related to the file
    pub fn from_file(path: &Path) -> Result<FileStoreKey, Error> {
        let mut hasher = Blake2b::new();
        let file_reader = ReadFileIterator::new(path)?;
        file_reader.map(|buf| hasher.input(&buf)).last();
        Ok(FileStoreKey {
            hash: hasher.result().to_vec(),
        })
    }
}

impl std::string::ToString for FileStoreKey {
    fn to_string(&self) -> String {
        hex::encode(&self.hash)
    }
}

impl std::fmt::Debug for FileStoreKey {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        fmt.write_str(&hex::encode(&self.hash))
    }
}

impl FileStoreItem {
    /// Make a new FileStoreItem
    fn new() -> FileStoreItem {
        FileStoreItem {
            persistent: Utc::now(),
        }
    }

    /// Mark the file as persistent
    fn persist(&mut self) {
        let now = Utc::now().timestamp();
        let target = now + (PERSISTENCY_DURATION.as_secs() as i64);
        self.persistent = DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp(target, 0), Utc);
    }
}

impl FileStoreData {
    /// Make a new FileStoreData
    fn new() -> FileStoreData {
        FileStoreData {
            items: HashMap::new(),
        }
    }

    /// Get a mutable reference to the item with that key, creating it if
    /// needed
    fn get_mut(&mut self, key: &FileStoreKey) -> &mut FileStoreItem {
        let key = key.to_string();
        if !self.items.contains_key(&key) {
            self.items.insert(key.clone(), FileStoreItem::new());
        }
        self.items.get_mut(&key).unwrap()
    }

    /// Remove an item from the list of know files. This wont remove the actual
    /// file on disk
    fn remove(&mut self, key: &FileStoreKey) -> Option<FileStoreItem> {
        self.items.remove(&key.to_string())
    }
}

impl FileStore {
    /// Make a new FileStore in the specified base directory, will lock if
    /// another instance of a FileStore is locking the data file.
    pub fn new(base_path: &Path) -> Result<FileStore, Error> {
        std::fs::create_dir_all(base_path)?;
        let path = Path::new(base_path).join("store_info");
        if !path.exists() {
            serde_json::to_writer(File::create(&path)?, &FileStoreData::new())?;
        }
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        if let Err(e) = file.try_lock_exclusive() {
            if e.to_string() != fs2::lock_contended_error().to_string() {
                return Err(e.into());
            }
            warn!("Store locked... waiting");
            file.lock_exclusive()?;
        }
        let data = FileStore::read_store_file(&file, base_path)?;
        Ok(FileStore {
            base_path: base_path.to_owned(),
            file,
            data,
        })
    }

    /// Given an iterator of Vec<u8> consume all of it writing the content to
    /// the disk if the file is not already present on disk. The file is stored
    /// inside the base directory and chmod -w
    pub fn store<I>(&mut self, key: &FileStoreKey, content: I) -> Result<(), Error>
    where
        I: Iterator<Item = Vec<u8>>,
    {
        let path = self.key_to_path(key);
        trace!("Storing {:?}", path);
        if self.has_key(key) {
            trace!("File {:?} already exists", path);
            content.last(); // consume all the iterator
            self.data.get_mut(key).persist();
            self.flush()?;
            return Ok(());
        }
        // TODO make write the file to a .temp and then move to the final place?
        // not sure if needed since this is in a &mut self and should not be executed
        // in parallel even between processes
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut file = std::fs::File::create(&path)?;
        content.map(|data| file.write_all(&data)).last();
        FileStore::mark_readonly(&path)?;
        self.data.get_mut(key).persist();
        self.flush()?;
        Ok(())
    }

    /// Returns the path of the file with that key or None if it's not in the
    /// FileStore
    pub fn get(&mut self, key: &FileStoreKey) -> Result<Option<PathBuf>, Error> {
        let path = self.key_to_path(key);
        if !path.exists() {
            self.data.remove(&key);
            self.flush()?;
            return Ok(None);
        }
        if CHECK_INTEGRITY {
            if !self.check_integrity(key) {
                warn!("File {:?} failed the integrity check", path);
                self.data.remove(key);
                FileStore::remove_file(&path)?;
                return Ok(None);
            }
        }
        self.persist(key)?;
        Ok(Some(path))
    }

    /// Checks if the store has that key inside. This may drop the file if it's
    /// corrupted
    pub fn has_key(&mut self, key: &FileStoreKey) -> bool {
        let path = self.key_to_path(key);
        if !path.exists() {
            return false;
        }
        if CHECK_INTEGRITY {
            if !self.check_integrity(&key) {
                warn!("File {:?} failed the integrity check", path);
                self.data.remove(key);
                FileStore::remove_file(&path).expect("Cannot remove corrupted file");
                return false;
            }
        }
        true
    }

    /// Mark the file as persistent
    pub fn persist(&mut self, key: &FileStoreKey) -> Result<(), Error> {
        let path = self.key_to_path(key);
        if !path.exists() {
            return Err(FileStoreError::NotFound.into());
        }
        self.data.get_mut(key).persist();
        self.flush()?;
        Ok(())
    }

    /// Write the FileStore data to disk
    pub fn flush(&mut self) -> Result<(), Error> {
        let serialized = serde_json::to_string(&self.data)?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(serialized.as_bytes())?;
        self.file.set_len(serialized.len() as u64)?;
        Ok(())
    }

    /// Path of the file to disk
    fn key_to_path(&self, key: &FileStoreKey) -> PathBuf {
        let first = hex::encode(vec![key.hash[0]]);
        let second = hex::encode(vec![key.hash[1]]);
        let full = hex::encode(&key.hash);
        Path::new(&self.base_path)
            .join(first)
            .join(second)
            .join(full)
            .to_owned()
    }

    /// Read the FileStore data file from disk
    fn read_store_file(file: &File, base_path: &Path) -> Result<FileStoreData, Error> {
        let mut data: FileStoreData = serde_json::from_reader(file)?;
        // remove files not present anymore
        data.items = data
            .items
            .into_iter()
            .filter(|(key, _)| {
                base_path
                    .join(&key[0..2])
                    .join(&key[2..4])
                    .join(key)
                    .exists()
            })
            .collect();
        Ok(data)
    }

    /// Mark a file as readonly
    fn mark_readonly(path: &Path) -> Result<(), Error> {
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(path, perms)?;
        Ok(())
    }

    /// Remove a file from disk
    fn remove_file(path: &Path) -> Result<(), Error> {
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms)?;
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// Check if the file is not corrupted
    fn check_integrity(&self, key: &FileStoreKey) -> bool {
        let path = self.key_to_path(key);
        let metadata = std::fs::metadata(&path);
        // if the last modified time is the same of creation time assume it's
        // not corrupted
        if let Ok(metadata) = metadata {
            let created = metadata.created();
            let modified = metadata.modified();
            match (created, modified) {
                (Ok(created), Ok(modified)) => {
                    if created == modified {
                        return true;
                    }
                }
                (_, _) => {}
            }
        }
        match FileStoreKey::from_file(&path) {
            Ok(key2) => key2.hash == key.hash,
            Err(_) => false,
        }
    }
}
