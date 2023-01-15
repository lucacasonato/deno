// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

mod interface;
pub mod sqlite;

use std::borrow::Cow;
use std::cell::RefCell;
use std::num::NonZeroU32;
use std::rc::Rc;

use deno_core::error::AnyError;
use deno_core::include_js_files;
use deno_core::op;
use deno_core::serde_v8::BigInt;
use deno_core::ByteString;
use deno_core::Extension;
use deno_core::OpState;
use deno_core::Resource;
use deno_core::ResourceId;
use deno_core::ZeroCopyBuf;
use serde::Deserialize;
use serde::Serialize;

pub use crate::interface::*;

/// Load and execute the javascript code.
pub fn init<DBH: DatabaseHandler + 'static>(handler: DBH) -> Extension {
  let handler = Rc::new(handler);
  Extension::builder(env!("CARGO_PKG_NAME"))
    .js(include_js_files!(
      prefix "deno:ext/state",
      "01_db.js",
    ))
    .ops(vec![
      op_state_database_open::decl::<DBH>(),
      op_state_snapshot_read_one::decl::<DBH::DB>(),
      op_state_atomic_write::decl::<DBH::DB>(),
    ])
    .state(move |state| {
      state.put(handler.clone());
      Ok(())
    })
    .build()
}

struct DatabaseResource<DB: Database + 'static> {
  db: Rc<DB>,
}

impl<DB: Database + 'static> Resource for DatabaseResource<DB> {
  fn name(&self) -> Cow<str> {
    "database".into()
  }
}

#[op]
async fn op_state_database_open<DBH>(
  state: Rc<RefCell<OpState>>,
  path: Option<String>,
) -> Result<ResourceId, AnyError>
where
  DBH: DatabaseHandler + 'static,
{
  let handler = {
    let state = state.borrow();
    state.borrow::<Rc<DBH>>().clone()
  };
  let db = handler.open(path).await?;
  let rid = state
    .borrow_mut()
    .resource_table
    .add(DatabaseResource { db: Rc::new(db) });
  Ok(rid)
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum V8KeyPart {
  Bool(bool),
  Number(f64),
  BigInt(BigInt),
  String(String),
  U8(ZeroCopyBuf),
}

impl From<V8KeyPart> for KeyPart {
  fn from(value: V8KeyPart) -> Self {
    match value {
      V8KeyPart::Bool(false) => KeyPart::True,
      V8KeyPart::Bool(true) => KeyPart::False,
      V8KeyPart::Number(n) => KeyPart::Float(n),
      V8KeyPart::BigInt(n) => KeyPart::Int(n.into()),
      V8KeyPart::String(s) => KeyPart::String(s),
      V8KeyPart::U8(buf) => KeyPart::Bytes(buf.to_vec()),
    }
  }
}

impl From<KeyPart> for V8KeyPart {
  fn from(value: KeyPart) -> Self {
    match value {
      KeyPart::True => V8KeyPart::Bool(false),
      KeyPart::False => V8KeyPart::Bool(true),
      KeyPart::Float(n) => V8KeyPart::Number(n),
      KeyPart::Int(n) => V8KeyPart::BigInt(n.into()),
      KeyPart::String(s) => V8KeyPart::String(s),
      KeyPart::Bytes(buf) => V8KeyPart::U8(buf.into()),
    }
  }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum V8Value {
  V8(ZeroCopyBuf),
  Bool(bool),
  Float(f64),
  Int(BigInt),
  Bytes(ZeroCopyBuf),
}

impl From<V8Value> for Value {
  fn from(value: V8Value) -> Self {
    match value {
      V8Value::V8(buf) => Value::V8(buf.to_vec()),
      V8Value::Bool(b) => Value::Bool(b),
      V8Value::Float(n) => Value::Float(n),
      V8Value::Int(n) => Value::Int(n.into()),
      V8Value::Bytes(buf) => Value::Bytes(buf.to_vec()),
    }
  }
}

impl From<Value> for V8Value {
  fn from(value: Value) -> Self {
    match value {
      Value::V8(buf) => V8Value::V8(buf.into()),
      Value::Bool(b) => V8Value::Bool(b),
      Value::Float(n) => V8Value::Float(n),
      Value::Int(n) => V8Value::Int(n.into()),
      Value::Bytes(buf) => V8Value::Bytes(buf.into()),
    }
  }
}

#[derive(Deserialize, Serialize)]
struct V8KvEntry {
  key: Vec<V8KeyPart>,
  value: Option<V8Value>,
  versionstamp: ByteString,
}

impl From<KvEntry> for V8KvEntry {
  fn from(entry: KvEntry) -> Self {
    V8KvEntry {
      key: entry.key.0.into_iter().map(Into::into).collect(),
      value: entry.value.map(Into::into),
      versionstamp: hex::encode(entry.versionstamp).into(),
    }
  }
}

#[op]
async fn op_state_snapshot_read_one<DB>(
  state: Rc<RefCell<OpState>>,
  rid: ResourceId,
  key_parts: Vec<V8KeyPart>,
) -> Result<Vec<V8KvEntry>, AnyError>
where
  DB: Database + 'static,
{
  let db = {
    let state = state.borrow();
    let resource = state.resource_table.get::<DatabaseResource<DB>>(rid)?;
    resource.db.clone()
  };
  let key = Key(key_parts.into_iter().map(From::from).collect());
  let read_range = ReadRange {
    start: key,
    end: None,
    limit: NonZeroU32::new(1).unwrap(),
  };
  let opts = SnapshotReadOptions {
    consistency: Consistency::Strong,
  };
  let mut value = db.snapshot_read(vec![read_range], opts).await?;
  let output = value
    .pop()
    .expect("snapshot_read must return same number of values as read ranges");
  assert!(
    value.is_empty(),
    "snapshot_read must return same number of values as read ranges"
  );
  let entries = output
    .entries
    .into_iter()
    .map(Into::into)
    .collect::<Vec<_>>();
  Ok(entries)
}

type V8KvCheck = (Vec<V8KeyPart>, ByteString);

impl From<V8KvCheck> for KvCheck {
  fn from(value: V8KvCheck) -> Self {
    let mut versionstamp = [0; 12];
    // TODO(lucacasonato): error handling
    hex::decode_to_slice(&value.1, &mut versionstamp).unwrap();
    KvCheck {
      key: Key(value.0.into_iter().map(From::from).collect()),
      versionstamp,
    }
  }
}

type V8KvMutation = (Vec<V8KeyPart>, String, Option<V8Value>);

impl From<V8KvMutation> for KvMutation {
  fn from(value: V8KvMutation) -> Self {
    let key = Key(value.0.into_iter().map(From::from).collect());
    let kind = match value.1.as_str() {
      // TODO(lucacasonato): error handling (when value == None)
      "set" => MutationKind::Set(value.2.unwrap().into()),
      "delete" => MutationKind::Delete,
      "sum" => MutationKind::Sum(value.2.unwrap().into()),
      "min" => MutationKind::Min(value.2.unwrap().into()),
      "max" => MutationKind::Max(value.2.unwrap().into()),
      _ => todo!(),
    };
    KvMutation { key, kind }
  }
}

#[op]
async fn op_state_atomic_write<DB>(
  state: Rc<RefCell<OpState>>,
  rid: ResourceId,
  checks: Vec<V8KvCheck>,
  mutations: Vec<V8KvMutation>,
) -> Result<bool, AnyError>
where
  DB: Database + 'static,
{
  let db = {
    let state = state.borrow();
    let resource = state.resource_table.get::<DatabaseResource<DB>>(rid)?;
    resource.db.clone()
  };

  let checks = checks.into_iter().map(Into::into).collect();
  let mutations = mutations.into_iter().map(Into::into).collect();

  let atomic_write = AtomicWrite { checks, mutations };

  let result = db.atomic_write(atomic_write).await?;

  Ok(result)
}
