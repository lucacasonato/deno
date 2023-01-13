// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

pub mod fake;

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use async_trait::async_trait;
use deno_core::error::AnyError;
use deno_core::include_js_files;
use deno_core::op;
use deno_core::Extension;
use deno_core::OpState;
use deno_core::Resource;
use deno_core::ResourceId;

/// Load and execute the javascript code.
pub fn init<DBH: DatabaseHandler + 'static>(handler: DBH) -> Extension {
  let handler = Rc::new(handler);
  Extension::builder(env!("CARGO_PKG_NAME"))
    .js(include_js_files!(
      prefix "deno:ext/state",
      "01_db.js",
    ))
    .ops(vec![op_state_database_open::decl::<DBH>()])
    .state(move |state| {
      state.put(handler.clone());
      Ok(())
    })
    .build()
}

#[async_trait]
pub trait DatabaseHandler {
  type DB: Database + 'static;

  async fn open(&self, path: Option<String>) -> Result<Self::DB, AnyError>;
}

#[async_trait]
pub trait Database {}

struct DatabaseResource<DB: Database + 'static> {
  db: DB,
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
    .add(DatabaseResource { db });
  Ok(rid)
}
