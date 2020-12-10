// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.

use super::analysis;
use super::config::Config;
use super::diagnostics::DiagnosticCollection;
use super::memory_cache::MemoryCache;
use super::sources::Sources;
use super::tsc;
use super::utils::notification_is;

use crate::deno_dir;
use crate::import_map::ImportMap;
use crate::media_type::MediaType;

use crossbeam_channel::Sender;
use deno_core::error::anyhow;
use deno_core::error::AnyError;
use deno_core::url::Url;
use deno_core::JsRuntime;
use deno_core::ModuleSpecifier;
use lsp_server::Message;
use lsp_server::Notification;
use lsp_server::Request;
use lsp_server::RequestId;
use lsp_server::Response;
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;

type ReqHandler = fn(&mut ServerState, Response);
type ReqQueue = lsp_server::ReqQueue<(String, Instant), ReqHandler>;

pub fn update_import_map(state: &mut ServerState) -> Result<(), AnyError> {
  if let Some(import_map_str) = &state.config.settings.import_map {
    let import_map_url = if let Ok(url) = Url::from_file_path(import_map_str) {
      Ok(url)
    } else if let Some(root_uri) = &state.config.root_uri {
      let root_path = root_uri
        .to_file_path()
        .map_err(|_| anyhow!("Bad root_uri: {}", root_uri))?;
      let import_map_path = root_path.join(import_map_str);
      Url::from_file_path(import_map_path).map_err(|_| {
        anyhow!("Bad file path for import map: {:?}", import_map_str)
      })
    } else {
      Err(anyhow!(
        "The path to the import map (\"{}\") is not resolvable.",
        import_map_str
      ))
    }?;
    let import_map_path = import_map_url
      .to_file_path()
      .map_err(|_| anyhow!("Bad file path."))?;
    let import_map_json =
      fs::read_to_string(import_map_path).map_err(|err| {
        anyhow!(
          "Failed to load the import map at: {}. [{}]",
          import_map_url,
          err
        )
      })?;
    let import_map =
      ImportMap::from_json(&import_map_url.to_string(), &import_map_json)?;
    state.maybe_import_map_uri = Some(import_map_url);
    state.maybe_import_map = Some(Arc::new(RwLock::new(import_map)));
  } else {
    state.maybe_import_map = None;
  }
  Ok(())
}

pub enum Event {
  Message(Message),
}

impl fmt::Debug for Event {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    let debug_verbose_not =
      |notification: &Notification, f: &mut fmt::Formatter| {
        f.debug_struct("Notification")
          .field("method", &notification.method)
          .finish()
      };

    match self {
      Event::Message(Message::Notification(notification)) => {
        if notification_is::<lsp_types::notification::DidOpenTextDocument>(
          notification,
        ) || notification_is::<lsp_types::notification::DidChangeTextDocument>(
          notification,
        ) {
          return debug_verbose_not(notification, f);
        }
      }
      _ => (),
    }
    match self {
      Event::Message(it) => fmt::Debug::fmt(it, f),
    }
  }
}

#[derive(Eq, PartialEq, Copy, Clone)]
pub enum Status {
  Loading,
  Ready,
}

impl Default for Status {
  fn default() -> Self {
    Status::Loading
  }
}

#[derive(Debug, Clone)]
pub struct DocumentData {
  pub dependencies: Option<HashMap<String, analysis::Dependency>>,
  pub version: Option<i32>,
  specifier: ModuleSpecifier,
}

impl DocumentData {
  pub fn new(
    specifier: ModuleSpecifier,
    version: i32,
    source: &str,
    maybe_import_map: Option<Arc<RwLock<ImportMap>>>,
  ) -> Self {
    let dependencies = if let Some((dependencies, _)) =
      analysis::analyze_dependencies(
        &specifier,
        source,
        &MediaType::from(&specifier),
        maybe_import_map,
      ) {
      Some(dependencies)
    } else {
      None
    };
    Self {
      dependencies,
      version: Some(version),
      specifier,
    }
  }

  pub fn update(
    &mut self,
    version: i32,
    source: &str,
    maybe_import_map: Option<Arc<RwLock<ImportMap>>>,
  ) {
    self.dependencies = if let Some((dependencies, _)) =
      analysis::analyze_dependencies(
        &self.specifier,
        source,
        &MediaType::from(&self.specifier),
        maybe_import_map,
      ) {
      Some(dependencies)
    } else {
      None
    };
    self.version = Some(version)
  }
}

/// An immutable snapshot of the server state at a point in time.
#[derive(Debug, Clone, Default)]
pub struct ServerStateSnapshot {
  pub config: Config,
  pub diagnostics: DiagnosticCollection,
  pub doc_data: HashMap<ModuleSpecifier, DocumentData>,
  pub file_cache: Arc<RwLock<MemoryCache>>,
  pub sources: Arc<RwLock<Sources>>,
}

pub struct ServerState {
  pub config: Config,
  pub diagnostics: Arc<Mutex<DiagnosticCollection>>,
  pub doc_data: HashMap<ModuleSpecifier, DocumentData>,
  pub file_cache: Arc<RwLock<MemoryCache>>,
  pub maybe_import_map: Option<Arc<RwLock<ImportMap>>>,
  pub maybe_import_map_uri: Option<Url>,
  pub req_queue: Arc<Mutex<ReqQueue>>,
  pub sender: Sender<Message>,
  pub sources: Arc<RwLock<Sources>>,
  pub shutdown_requested: bool,
  pub status: Status,
  pub ts_runtime: JsRuntime,
}

impl ServerState {
  pub fn new(sender: Sender<Message>, config: Config) -> Self {
    let custom_root = env::var("DENO_DIR").map(String::into).ok();
    let dir =
      deno_dir::DenoDir::new(custom_root).expect("could not access DENO_DIR");
    let location = dir.root.join("deps");
    let sources = Sources::new(&location);
    // TODO(@kitsonk) we need to allow displaying diagnostics here, but the
    // current compiler snapshot sends them to stdio which would totally break
    // the language server...
    let ts_runtime = tsc::start(false).expect("could not start tsc");

    Self {
      config,
      diagnostics: Default::default(),
      doc_data: Default::default(),
      file_cache: Arc::new(RwLock::new(Default::default())),
      maybe_import_map: None,
      maybe_import_map_uri: None,
      req_queue: Default::default(),
      sender,
      sources: Arc::new(RwLock::new(sources)),
      shutdown_requested: false,
      status: Default::default(),
      ts_runtime,
    }
  }

  pub fn cancel(
    req_queue: Arc<Mutex<ReqQueue>>,
    sender: Sender<Message>,
    request_id: RequestId,
  ) {
    let mut req_queue = req_queue.lock().unwrap();
    if let Some(response) = req_queue.incoming.cancel(request_id) {
      sender.send(response.into()).unwrap();
    }
  }

  pub fn complete_request(&mut self, response: Response) {
    let handler = {
      let mut req_queue = self.req_queue.lock().unwrap();
      req_queue.outgoing.complete(response.id.clone())
    };
    handler(self, response)
  }

  pub async fn next_event(
    &self,
    inbox: &mut UnboundedReceiver<Message>,
  ) -> Option<Event> {
    inbox.recv().await.map(Event::Message)
  }

  /// Handle any changes and return a `bool` that indicates if there were
  /// important changes to the state.
  pub fn process_changes(&mut self) -> bool {
    let mut file_cache = self.file_cache.write().unwrap();
    let changed_files = file_cache.take_changes();
    // other processing of changed files should be done here as needed
    !changed_files.is_empty()
  }

  pub fn register_request(&self, request: &Request, received: Instant) {
    let mut req_queue = self.req_queue.lock().unwrap();
    req_queue
      .incoming
      .register(request.id.clone(), (request.method.clone(), received));
  }

  pub fn respond(
    req_queue: Arc<Mutex<ReqQueue>>,
    sender: Sender<Message>,
    response: Response,
  ) {
    let mut req_queue = req_queue.lock().unwrap();
    if let Some((_, _)) = req_queue.incoming.complete(response.id.clone()) {
      sender.send(response.into()).unwrap();
    }
  }

  pub fn send_notification<N: lsp_types::notification::Notification>(
    sender: Sender<Message>,
    params: N::Params,
  ) {
    let notification = Notification::new(N::METHOD.to_string(), params);
    sender.send(notification.into()).unwrap();
  }

  pub fn send_request<R: lsp_types::request::Request>(
    req_queue: Arc<Mutex<ReqQueue>>,
    sender: Sender<Message>,
    params: R::Params,
    handler: ReqHandler,
  ) {
    let mut req_queue = req_queue.lock().unwrap();
    let request =
      req_queue
        .outgoing
        .register(R::METHOD.to_string(), params, handler);
    sender.send(request.into()).unwrap();
  }

  pub fn snapshot(&self) -> ServerStateSnapshot {
    ServerStateSnapshot {
      config: self.config.clone(),
      diagnostics: self.diagnostics.lock().unwrap().clone(),
      doc_data: self.doc_data.clone(),
      file_cache: Arc::clone(&self.file_cache),
      sources: Arc::clone(&self.sources),
    }
  }

  pub fn transition(&mut self, new_status: Status) {
    self.status = new_status;
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use deno_core::serde_json::json;
  use deno_core::serde_json::Value;
  use lsp_server::Connection;
  use tempfile::TempDir;

  #[test]
  fn test_update_import_map() {
    let temp_dir = TempDir::new().expect("could not create temp dir");
    let import_map_path = temp_dir.path().join("import_map.json");
    let import_map_str = &import_map_path.to_string_lossy();
    fs::write(
      import_map_path.clone(),
      r#"{
      "imports": {
        "denoland/": "https://deno.land/x/"
      }
    }"#,
    )
    .expect("could not write file");
    let mut config = Config::default();
    config
      .update(json!({
        "enable": false,
        "config": Value::Null,
        "lint": false,
        "importMap": import_map_str,
        "unstable": true,
      }))
      .expect("could not update config");
    let (connection, _) = Connection::memory();
    let mut state = ServerState::new(connection.sender, config);
    let result = update_import_map(&mut state);
    assert!(result.is_ok());
    assert!(state.maybe_import_map.is_some());
    let expected =
      Url::from_file_path(import_map_path).expect("could not parse url");
    assert_eq!(state.maybe_import_map_uri, Some(expected));
    let import_map = state.maybe_import_map.unwrap();
    let import_map = import_map.read().unwrap();
    assert_eq!(
      import_map
        .resolve("denoland/mod.ts", "https://example.com/index.js")
        .expect("bad response"),
      Some(
        ModuleSpecifier::resolve_url("https://deno.land/x/mod.ts")
          .expect("could not create URL")
      )
    );
  }
}
