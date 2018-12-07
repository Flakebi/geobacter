use std::collections::{HashMap as StdHashMap, HashSet as StdHashSet, };
use std::error::Error;
use std::fs::{create_dir_all, File, };
use std::hash::{BuildHasherDefault, Hash, };
use std::io;
use std::ops::{Deref, DerefMut, };
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::Sender;

use seahash::{SeaHasher, };

use fs2::FileExt;

pub mod git;

pub type HashMap<K, V> = StdHashMap<K, V, BuildHasherDefault<SeaHasher>>;
pub type HashSet<K> = StdHashSet<K, BuildHasherDefault<SeaHasher>>;

/// HashSet doesn't implement `Default` w/ custom hashers, for some reason.
pub fn new_hash_set<K>() -> HashSet<K>
  where K: Eq + Hash,
{
  HashSet::with_hasher(BuildHasherDefault::default())
}

pub trait CreateIfNotExists: AsRef<Path> {
  fn create_if_not_exists(&self) -> io::Result<()> {
    let p = self.as_ref();
    if !p.exists() {
      create_dir_all(p)?;
    }

    Ok(())
  }
}

impl CreateIfNotExists for PathBuf { }
impl<'a> CreateIfNotExists for &'a Path { }

pub fn run_cmd(mut cmd: Command) -> Result<(), Box<Error>> {
  info!("running command {:?}", cmd);
  let mut child = cmd.spawn()?;
  if !child.wait()?.success() {
    Err(format!("command failed: {:?}", cmd).into())
  } else {
    Ok(())
  }
}

/// DO NOT SEND ON THIS SENDER. Only send on a thread local
/// clone of the sender in this obj.
/// `Sender` doesn't implement `Sync`, but we want to avoid cross thread comms
/// just to get a local copy of the sender. So we force `Sync` here and require
/// that no attempts to send on the sender are made on the shared copy.
pub struct UnsafeSyncSender<T>(pub(crate) Sender<T>);
impl<T> UnsafeSyncSender<T> {
  pub fn clone_into<U>(&self) -> U
    where U: From<Self>,
  {
    U::from(self.clone())
  }
}
impl<T> Clone for UnsafeSyncSender<T> {
  fn clone(&self) -> Self {
    UnsafeSyncSender(self.0.clone())
  }
}
unsafe impl<T> Sync for UnsafeSyncSender<T> { }

pub struct FileLockGuard(File);
impl FileLockGuard {
  pub fn enter_create<T>(file: T) -> Result<Self, Box<Error>>
    where T: AsRef<Path>,
  {
    let file = File::create(file)?;
    file.lock_exclusive()?;
    Ok(FileLockGuard(file))
  }
}

impl Deref for FileLockGuard {
  type Target = File;
  fn deref(&self) -> &File { &self.0 }
}
impl DerefMut for FileLockGuard {
  fn deref_mut(&mut self) -> &mut File { &mut self.0 }
}
impl Drop for FileLockGuard {
  fn drop(&mut self) {
    match self.0.unlock() {
      Ok(_) => {},
      Err(e) => {
        error!("failed to unlock file in guard drop: {}", e);
      }
    }
  }
}