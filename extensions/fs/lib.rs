use std::path::Path;
use std::path::PathBuf;

use async_trait::async_trait;
use deno_core::error::AnyError;
use deno_core::Extension;
use deno_web::BlobPart;

pub trait FileSystemPermissions {
  fn check_read(&mut self, path: &Path) -> Result<(), AnyError>;
  fn check_write(&mut self, path: &Path) -> Result<(), AnyError>;
}

pub enum FileSystemHandleKind {
  File,
  Directory,
}

pub struct FileSystemHandle {
  pub kind: FileSystemHandleKind,
  pub path: PathBuf,
}

/// A generic trait that implements writing a file.
#[async_trait]
pub trait WritableFile {
  async fn write(&self, data: [u8]) -> Result<(), AnyError>;
  async fn seek(&self, position: u64) -> Result<(), AnyError>;
  async fn truncate(&self, size: u64) -> Result<(), AnyError>;
}

/// A generic abstraction of the file system.
#[async_trait]
pub trait FileSystem {
  /// Create a file blob for a given path.
  async fn create_file_blob(
    &self,
    path: &Path,
  ) -> Result<Box<dyn BlobPart + Send + Sync>, AnyError>;

  /// Create a file writable for a given path.
  async fn create_file_writable(
    &self,
    path: &Path,
  ) -> Result<Box<dyn WritableFile + Send + Sync>, AnyError>;

  /// List the directory specified by a given path.
  async fn list_directory(&self, path: &Path)
    -> Result<Vec<PathBuf>, AnyError>;

  /// Remove a specific entry (file or directory) at a given path.
  async fn remove(&self, path: &Path, recursive: bool) -> Result<(), AnyError>;
}

pub fn init<FS: FileSystem, P: FileSystemPermissions>(fs: FS) -> Extension {
  Extension::builder().build()
}

struct FileBlobPart {
  path: PathBuf,
}

impl BlobPart for FileBlobPart {
  async fn read(&self) -> Result<&[u8], AnyError> {
    todo!()
  }

  fn size(&self) -> usize {
    std::fs::metadata(self.path)
  }
}
