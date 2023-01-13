use async_trait::async_trait;
use deno_core::error::AnyError;

use crate::Database;
use crate::DatabaseHandler;

pub struct FakeDbHandler;

#[async_trait]
impl DatabaseHandler for FakeDbHandler {
  type DB = FakeDb;

  async fn open(&self, _path: Option<String>) -> Result<Self::DB, AnyError> {
    Ok(FakeDb)
  }
}

pub struct FakeDb;

#[async_trait]
impl Database for FakeDb {}
