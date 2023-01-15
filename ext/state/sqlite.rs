use std::borrow::Cow;
use std::cell::RefCell;
use std::num::NonZeroU32;
use std::path::Path;
use std::path::PathBuf;

use async_trait::async_trait;
use deno_core::error::AnyError;
use rusqlite::params;
use rusqlite::OptionalExtension;

use crate::AtomicWrite;
use crate::Database;
use crate::DatabaseHandler;
use crate::Key;
use crate::KvEntry;
use crate::MutationKind;
use crate::ReadRange;
use crate::ReadRangeOutput;
use crate::SnapshotReadOptions;

const STATEMENT_INC_AND_GET_DATA_VERSION: &str =
  "update data_version set version = version + 1 where k = 0 returning version";
const STATEMENT_KV_POINT_GET: &str =
  "select v, v_encoding, version from kv where k = ?";
const STATEMENT_KV_POINT_GET_VERSION_ONLY: &str =
  "select version from kv where k = ?";
const STATEMENT_KV_POINT_SET: &str =
  "insert into kv (k, v, v_encoding, version) values (:k, :v, :v_encoding, :version) on conflict(k) do update set v = :v, v_encoding = :v_encoding, version = :version";
const STATEMENT_KV_POINT_DELETE: &str = "delete from kv where k = ?";

const STATEMENT_CREATE_MIGRATION_TABLE: &str = "
create table if not exists migration_state(
  k integer not null primary key,
  version integer not null
)
";

const MIGRATIONS: [&str; 2] = [
  "
create table data_version (
  k integer primary key,
  version integer not null
);
insert into data_version (k, version) values (0, 0);
create table kv (
  k blob primary key,
  v blob not null,
  v_encoding integer not null,
  version integer not null
) without rowid;
",
  "
create table queue (
  ts integer not null,
  id text not null,
  data text not null,
  remaining_attempts integer not null,

  primary key (ts, id)
);
create table queue_running(
  deadline integer not null,
  id text not null,
  data text not null,
  remaining_attempts integer not null,

  primary key (deadline, id)
);
create table queue_undelivered(
  ts integer not null,
  id text not null,
  data text not null,

  primary key (ts, id)
);
",
];

pub struct SqliteDbHandler {
  pub default_storage_dir: Option<PathBuf>,
}

#[async_trait(?Send)]
impl DatabaseHandler for SqliteDbHandler {
  type DB = SqliteDb;

  async fn open(&self, path: Option<String>) -> Result<Self::DB, AnyError> {
    let conn = match (path, &self.default_storage_dir) {
      (Some(path), _) => {
        let path = PathBuf::from(path);
        // TODO: check permissions
        rusqlite::Connection::open(path)?
      }
      (None, Some(path)) => {
        std::fs::create_dir_all(&path)?;
        let path = path.join("state.sqlite3");
        rusqlite::Connection::open(&path)?
      }
      (None, None) => rusqlite::Connection::open_in_memory()?,
    };

    conn.pragma_update(None, "journal_mode", "wal")?;
    println!("enabled wal");
    conn.execute(STATEMENT_CREATE_MIGRATION_TABLE, [])?;
    println!("created migration table");

    let current_version: usize = conn
      .query_row(
        "select version from migration_state where k = 0",
        [],
        |row| row.get(0),
      )
      .optional()?
      .unwrap_or(0);
    println!("db version: {}", current_version);

    for (i, migration) in MIGRATIONS.iter().enumerate() {
      let version = i + 1;
      if version > current_version {
        conn.execute_batch(migration)?;
        println!("ran migration {}", version);
        conn.execute(
          "replace into migration_state (k, version) values(?, ?)",
          &[&0, &version],
        )?;
        println!("updated migration version to {}", version);
      }
    }

    Ok(SqliteDb(RefCell::new(conn)))
  }
}

pub struct SqliteDb(RefCell<rusqlite::Connection>);

#[async_trait(?Send)]
impl Database for SqliteDb {
  async fn snapshot_read(
    &self,
    requests: Vec<ReadRange>,
    _options: SnapshotReadOptions,
  ) -> Result<Vec<ReadRangeOutput>, AnyError> {
    let mut responses = Vec::with_capacity(requests.len());

    for request in requests {
      const ONE: NonZeroU32 = unsafe { NonZeroU32::new_unchecked(1) };
      if request.end.is_none() && request.limit == ONE {
        let key = request.start;

        let db = self.0.borrow();

        let mut stmt = db.prepare_cached(STATEMENT_KV_POINT_GET)?;
        let row = stmt
          .query_row(&[encode_key(&key)], |row| {
            let value: Vec<u8> = row.get(0)?;
            let encoding: i64 = row.get(1)?;

            let value = decode_value(value, encoding);

            let version: i64 = row.get(2)?;

            Ok((value, version))
          })
          .optional()?;

        let (value, version) = row.unzip();

        let version = version.unwrap_or(0);

        let entry = KvEntry {
          key,
          value,
          versionstamp: version_to_versionstamp(version),
        };

        responses.push(ReadRangeOutput {
          entries: vec![entry],
        });
      } else {
        todo!()
      }
    }

    Ok(responses)
  }

  async fn atomic_write(&self, write: AtomicWrite) -> Result<bool, AnyError> {
    let mut db = self.0.borrow_mut();

    let tx = db.transaction()?;

    for check in write.checks {
      let key = encode_key(&check.key);
      let version = tx
        .prepare_cached(STATEMENT_KV_POINT_GET_VERSION_ONLY)?
        .query_row(&[key], |row| row.get(0))
        .optional()?
        .unwrap_or(0);
      let real_versionstamp = version_to_versionstamp(version);
      if real_versionstamp != check.versionstamp {
        return Ok(false);
      }
    }

    let version: i64 = tx
      .prepare_cached(STATEMENT_INC_AND_GET_DATA_VERSION)?
      .query_row([], |row| row.get(0))?;

    for mutation in write.mutations {
      let key = encode_key(&mutation.key);

      match mutation.kind {
        MutationKind::Set(value) => {
          let (value, encoding) = encode_value(&value);
          tx.prepare_cached(STATEMENT_KV_POINT_SET)?
            .execute(params![key, &value, &encoding, &version])?;
        }
        MutationKind::Delete => {
          tx.prepare_cached(STATEMENT_KV_POINT_DELETE)?
            .execute(params![key])?;
        }
        MutationKind::Sum(_) => todo!(),
        MutationKind::Min(_) => todo!(),
        MutationKind::Max(_) => todo!(),
      }
    }

    tx.commit()?;

    Ok(true)
  }
}

/// TODO: properly encode
fn encode_key(key: &Key) -> &[u8] {
  let parts = &key.0;
  assert_eq!(parts.len(), 1);
  match &parts[0] {
    crate::KeyPart::String(key) => key.as_bytes(),
    _ => todo!(),
  }
}

/// TODO: properly decode
fn decode_key(key: Vec<u8>) -> Key {
  Key(vec![crate::KeyPart::String(
    String::from_utf8(key).unwrap(),
  )])
}

fn version_to_versionstamp(version: i64) -> [u8; 12] {
  let mut versionstamp = [0; 12];
  versionstamp[..8].copy_from_slice(&version.to_le_bytes());
  versionstamp
}

const VALUE_ENCODING_V8: i64 = 1;
const VALUE_ENCODING_BOOL: i64 = 2;
const VALUE_ENCODING_INT: i64 = 3;
const VALUE_ENCODING_FLOAT: i64 = 4;
const VALUE_ENCODING_BYTES: i64 = 5;

fn decode_value(value: Vec<u8>, encoding: i64) -> crate::Value {
  match encoding {
    VALUE_ENCODING_V8 => crate::Value::V8(value),
    VALUE_ENCODING_BOOL => crate::Value::Bool(value[0] != 0),
    VALUE_ENCODING_INT => todo!(),
    VALUE_ENCODING_FLOAT => todo!(),
    VALUE_ENCODING_BYTES => crate::Value::Bytes(value),
    _ => todo!(),
  }
}

fn encode_value(value: &crate::Value) -> (Cow<'_, [u8]>, i64) {
  match value {
    crate::Value::V8(value) => (Cow::Borrowed(value), VALUE_ENCODING_V8),
    crate::Value::Bool(value) => {
      (Cow::Owned(vec![*value as u8]), VALUE_ENCODING_BOOL)
    }
    crate::Value::Int(_value) => todo!(),
    crate::Value::Float(_value) => todo!(),
    crate::Value::Bytes(value) => (Cow::Borrowed(value), VALUE_ENCODING_BYTES),
  }
}
