// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use super::analysis::CodeLensSource;
use super::analysis::ResolvedDependency;
use super::analysis::ResolvedDependencyErr;
use super::language_server;
use super::language_server::StateSnapshot;
use super::text;
use super::text::LineIndex;

use crate::media_type::MediaType;
use crate::tokio_util::create_basic_runtime;
use crate::tsc;
use crate::tsc::ResolveArgs;
use crate::tsc_config::TsConfig;

use deno_core::error::anyhow;
use deno_core::error::custom_error;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::json_op_sync;
use deno_core::resolve_url;
use deno_core::serde::Deserialize;
use deno_core::serde::Serialize;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::url::Url;
use deno_core::JsRuntime;
use deno_core::ModuleSpecifier;
use deno_core::OpFn;
use deno_core::RuntimeOptions;
use lspower::lsp;
use regex::Captures;
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use text_size::TextSize;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::oneshot;

type Request = (
  RequestMethod,
  StateSnapshot,
  oneshot::Sender<Result<Value, AnyError>>,
);

type DiagnosticsRequest = (
  HashSet<ModuleSpecifier>,
  StateSnapshot,
  oneshot::Sender<Result<Option<Value>, AnyError>>,
);

#[derive(Clone, Debug)]
pub struct TsServer {
  requests_queue: mpsc::UnboundedSender<Request>,
  diagnostics_notifier: mpsc::Sender<()>,
  diagnostics_request: Arc<Mutex<Option<DiagnosticsRequest>>>,
}

impl TsServer {
  pub fn new() -> Self {
    let (requests_queue_tx, mut requests_queue_rx) =
      mpsc::unbounded_channel::<Request>();
    let (diagnostics_notifier_tx, mut diagnostics_notifier_rx) =
      mpsc::channel::<()>(1);
    let diagnostics_request: Arc<Mutex<Option<DiagnosticsRequest>>> =
      Default::default();

    let diagnostics_request_ = diagnostics_request.clone();

    let _join_handle = thread::spawn(move || {
      // TODO(@kitsonk) we need to allow displaying diagnostics here, but the
      // current compiler snapshot sends them to stdio which would totally break
      // the language server...
      let mut ts_runtime = start(false).expect("could not start tsc");

      let runtime = create_basic_runtime();
      runtime.block_on(async {
        // This loop handles incoming requests to TSC. Incoming requests need to
        // be prioritized somewhat. Regular requests (like hover or
        // completions) should be handled before diagnostic requests because
        // we want to respond to user interaction as fast as possible. We also
        // don't want to queue up stale diagnostic requests, and we want to
        // batch diagnostic requests as much as possible for performance
        // reasons. This tries to do that.
        let mut maybe_req: Option<Request> = None;
        loop {
          // First check if there is a regular request to handle.
          if let Some((req, state_snapshot, tx)) = maybe_req.take() {
            let res = request(&mut ts_runtime, state_snapshot, req);
            if tx.send(res).is_err() {
              warn!("Unable to send result to client.");
            }
          }

          // Then check if there is a regular request queued, if so place it in
          // maybe_req slot and jump back to start of loop to handle it.
          if let Some(recv_req) = requests_queue_rx.recv().now_or_never() {
            if let Some(req) = recv_req {
              maybe_req = Some(req);
              continue;
            } else {
              // The channel has closed, so we stop the loop.
              break;
            }
          }

          // Then check if there are diagnostics to handle.
          let maybe_diagnostics_request = {
            let mut requests = diagnostics_request.lock().unwrap();
            std::mem::take(&mut *requests)
          };
          if let Some((specifiers, state_snapshot, tx)) =
            maybe_diagnostics_request
          {
            let req =
              RequestMethod::GetDiagnostics(specifiers.into_iter().collect());
            let res = request(&mut ts_runtime, state_snapshot, req);
            if tx.send(res.map(Some)).is_err() {
              warn!("Unable to send result to client.");
            }
          }

          // If nothing is ready to be processed, we wait for the next regular
          // or diagnostic request.
          tokio::select! {
            recv_req = requests_queue_rx.recv() => {
              if let Some(req) = recv_req {
                maybe_req = Some(req);
                continue;
              } else {
                // The channel has closed, so we stop the loop.
                break;
              }
            }
            res = diagnostics_notifier_rx.recv() => {
              if res.is_none() {
                // The channel has closed, so we stop the loop.
                break;
              }
              continue;
            }
          }
        }
      })
    });

    Self {
      requests_queue: requests_queue_tx,
      diagnostics_notifier: diagnostics_notifier_tx,
      diagnostics_request: diagnostics_request_,
    }
  }

  pub async fn request(
    &self,
    snapshot: StateSnapshot,
    req: RequestMethod,
  ) -> Result<Value, AnyError> {
    let (tx, rx) = oneshot::channel::<Result<Value, AnyError>>();
    if self.requests_queue.send((req, snapshot, tx)).is_err() {
      return Err(anyhow!("failed to send request to tsc thread"));
    }
    rx.await?
  }

  pub async fn diagnostics_request(
    &self,
    snapshot: StateSnapshot,
    specifiers: Vec<ModuleSpecifier>,
  ) -> Result<Option<Value>, AnyError> {
    let (tx, rx) = oneshot::channel::<Result<Option<Value>, AnyError>>();
    {
      let mut request = self.diagnostics_request.lock().unwrap();
      let mut specifier_set =
        if let Some((set, _, original_rx)) = request.take() {
          original_rx.send(Ok(None)).unwrap();
          set
        } else {
          HashSet::new()
        };
      for specifier in specifiers {
        specifier_set.insert(specifier);
      }
      *request = Some((specifier_set, snapshot, tx));
    }
    // Wake up the tsc thread by notifying it, if it hasnt already been notified.
    //
    let res = self.diagnostics_notifier.try_send(());
    match res {
      Ok(_) => {}
      Err(TrySendError::Full(_)) => {
        // ignore - this means tsc is already scheduled to be notified.
      }
      Err(err) => return Err(err.into()),
    };
    rx.await.unwrap()
  }
}

/// An lsp representation of an asset in memory, that has either been retrieved
/// from static assets built into Rust, or static assets built into tsc.
#[derive(Debug, Clone)]
pub struct AssetDocument {
  pub text: String,
  pub length: usize,
  pub line_index: LineIndex,
}

impl AssetDocument {
  pub fn new<T: AsRef<str>>(text: T) -> Self {
    let text = text.as_ref();
    Self {
      text: text.to_string(),
      length: text.encode_utf16().count(),
      line_index: LineIndex::new(text),
    }
  }
}

#[derive(Debug, Clone)]
pub struct Assets(HashMap<ModuleSpecifier, Option<AssetDocument>>);

impl Default for Assets {
  fn default() -> Self {
    let assets = tsc::STATIC_ASSETS
      .iter()
      .map(|(k, v)| {
        let url_str = format!("asset:///{}", k);
        let specifier = resolve_url(&url_str).unwrap();
        let asset = AssetDocument::new(v);
        (specifier, Some(asset))
      })
      .collect();
    Self(assets)
  }
}

impl Assets {
  pub fn contains_key(&self, k: &ModuleSpecifier) -> bool {
    self.0.contains_key(k)
  }

  pub fn get(&self, k: &ModuleSpecifier) -> Option<&Option<AssetDocument>> {
    self.0.get(k)
  }

  pub fn insert(
    &mut self,
    k: ModuleSpecifier,
    v: Option<AssetDocument>,
  ) -> Option<Option<AssetDocument>> {
    self.0.insert(k, v)
  }
}

/// Optionally returns an internal asset, first checking for any static assets
/// in Rust, then checking any previously retrieved static assets from the
/// isolate, and then finally, the tsc isolate itself.
pub async fn get_asset(
  specifier: &ModuleSpecifier,
  ts_server: &TsServer,
  state_snapshot: StateSnapshot,
) -> Result<Option<AssetDocument>, AnyError> {
  let specifier_str = specifier.to_string().replace("asset:///", "");
  if let Some(text) = tsc::get_asset(&specifier_str) {
    let maybe_asset = Some(AssetDocument::new(text));
    Ok(maybe_asset)
  } else {
    let res = ts_server
      .request(state_snapshot, RequestMethod::GetAsset(specifier.clone()))
      .await?;
    let maybe_text: Option<String> = serde_json::from_value(res)?;
    let maybe_asset = if let Some(text) = maybe_text {
      Some(AssetDocument::new(text))
    } else {
      None
    };
    Ok(maybe_asset)
  }
}

fn display_parts_to_string(parts: Vec<SymbolDisplayPart>) -> String {
  parts
    .into_iter()
    .map(|p| p.text)
    .collect::<Vec<String>>()
    .join("")
}

fn get_tag_body_text(tag: &JSDocTagInfo) -> Option<String> {
  tag.text.as_ref().map(|text| match tag.name.as_str() {
    "example" => {
      let caption_regex =
        Regex::new(r"<caption>(.*?)</caption>\s*\r?\n((?:\s|\S)*)").unwrap();
      if caption_regex.is_match(&text) {
        caption_regex
          .replace(text, |c: &Captures| {
            format!("{}\n\n{}", &c[1], make_codeblock(&c[2]))
          })
          .to_string()
      } else {
        make_codeblock(text)
      }
    }
    "author" => {
      let email_match_regex = Regex::new(r"(.+)\s<([-.\w]+@[-.\w]+)>").unwrap();
      email_match_regex
        .replace(text, |c: &Captures| format!("{} {}", &c[1], &c[2]))
        .to_string()
    }
    "default" => make_codeblock(text),
    _ => replace_links(text),
  })
}

fn get_tag_documentation(tag: &JSDocTagInfo) -> String {
  match tag.name.as_str() {
    "augments" | "extends" | "param" | "template" => {
      if let Some(text) = &tag.text {
        let part_regex = Regex::new(r"^(\S+)\s*-?\s*").unwrap();
        let body: Vec<&str> = part_regex.split(&text).collect();
        if body.len() == 3 {
          let param = body[1];
          let doc = body[2];
          let label = format!("*@{}* `{}`", tag.name, param);
          if doc.is_empty() {
            return label;
          }
          if doc.contains('\n') {
            return format!("{}  \n{}", label, replace_links(doc));
          } else {
            return format!("{} - {}", label, replace_links(doc));
          }
        }
      }
    }
    _ => (),
  }
  let label = format!("*@{}*", tag.name);
  let maybe_text = get_tag_body_text(tag);
  if let Some(text) = maybe_text {
    if text.contains('\n') {
      format!("{}  \n{}", label, text)
    } else {
      format!("{} - {}", label, text)
    }
  } else {
    label
  }
}

fn make_codeblock(text: &str) -> String {
  let codeblock_regex = Regex::new(r"^\s*[~`]{3}").unwrap();
  if codeblock_regex.is_match(text) {
    text.to_string()
  } else {
    format!("```\n{}\n```", text)
  }
}

/// Replace JSDoc like links (`{@link http://example.com}`) with markdown links
fn replace_links(text: &str) -> String {
  let jsdoc_links_regex = Regex::new(r"(?i)\{@(link|linkplain|linkcode) (https?://[^ |}]+?)(?:[| ]([^{}\n]+?))?\}").unwrap();
  jsdoc_links_regex
    .replace_all(text, |c: &Captures| match &c[1] {
      "linkcode" => format!(
        "[`{}`]({})",
        if c.get(3).is_none() {
          &c[2]
        } else {
          c[3].trim()
        },
        &c[2]
      ),
      _ => format!(
        "[{}]({})",
        if c.get(3).is_none() {
          &c[2]
        } else {
          c[3].trim()
        },
        &c[2]
      ),
    })
    .to_string()
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub enum ScriptElementKind {
  #[serde(rename = "")]
  Unknown,
  #[serde(rename = "warning")]
  Warning,
  #[serde(rename = "keyword")]
  Keyword,
  #[serde(rename = "script")]
  ScriptElement,
  #[serde(rename = "module")]
  ModuleElement,
  #[serde(rename = "class")]
  ClassElement,
  #[serde(rename = "local class")]
  LocalClassElement,
  #[serde(rename = "interface")]
  InterfaceElement,
  #[serde(rename = "type")]
  TypeElement,
  #[serde(rename = "enum")]
  EnumElement,
  #[serde(rename = "enum member")]
  EnumMemberElement,
  #[serde(rename = "var")]
  VariableElement,
  #[serde(rename = "local var")]
  LocalVariableElement,
  #[serde(rename = "function")]
  FunctionElement,
  #[serde(rename = "local function")]
  LocalFunctionElement,
  #[serde(rename = "method")]
  MemberFunctionElement,
  #[serde(rename = "getter")]
  MemberGetAccessorElement,
  #[serde(rename = "setter")]
  MemberSetAccessorElement,
  #[serde(rename = "property")]
  MemberVariableElement,
  #[serde(rename = "constructor")]
  ConstructorImplementationElement,
  #[serde(rename = "call")]
  CallSignatureElement,
  #[serde(rename = "index")]
  IndexSignatureElement,
  #[serde(rename = "construct")]
  ConstructSignatureElement,
  #[serde(rename = "parameter")]
  ParameterElement,
  #[serde(rename = "type parameter")]
  TypeParameterElement,
  #[serde(rename = "primitive type")]
  PrimitiveType,
  #[serde(rename = "label")]
  Label,
  #[serde(rename = "alias")]
  Alias,
  #[serde(rename = "const")]
  ConstElement,
  #[serde(rename = "let")]
  LetElement,
  #[serde(rename = "directory")]
  Directory,
  #[serde(rename = "external module name")]
  ExternalModuleName,
  #[serde(rename = "JSX attribute")]
  JsxAttribute,
  #[serde(rename = "string")]
  String,
}

impl From<ScriptElementKind> for lsp::CompletionItemKind {
  fn from(kind: ScriptElementKind) -> Self {
    use lspower::lsp::CompletionItemKind;

    match kind {
      ScriptElementKind::PrimitiveType | ScriptElementKind::Keyword => {
        CompletionItemKind::Keyword
      }
      ScriptElementKind::ConstElement => CompletionItemKind::Constant,
      ScriptElementKind::LetElement
      | ScriptElementKind::VariableElement
      | ScriptElementKind::LocalVariableElement
      | ScriptElementKind::Alias => CompletionItemKind::Variable,
      ScriptElementKind::MemberVariableElement
      | ScriptElementKind::MemberGetAccessorElement
      | ScriptElementKind::MemberSetAccessorElement => {
        CompletionItemKind::Field
      }
      ScriptElementKind::FunctionElement => CompletionItemKind::Function,
      ScriptElementKind::MemberFunctionElement
      | ScriptElementKind::ConstructSignatureElement
      | ScriptElementKind::CallSignatureElement
      | ScriptElementKind::IndexSignatureElement => CompletionItemKind::Method,
      ScriptElementKind::EnumElement => CompletionItemKind::Enum,
      ScriptElementKind::ModuleElement
      | ScriptElementKind::ExternalModuleName => CompletionItemKind::Module,
      ScriptElementKind::ClassElement | ScriptElementKind::TypeElement => {
        CompletionItemKind::Class
      }
      ScriptElementKind::InterfaceElement => CompletionItemKind::Interface,
      ScriptElementKind::Warning | ScriptElementKind::ScriptElement => {
        CompletionItemKind::File
      }
      ScriptElementKind::Directory => CompletionItemKind::Folder,
      ScriptElementKind::String => CompletionItemKind::Constant,
      _ => CompletionItemKind::Property,
    }
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TextSpan {
  pub start: u32,
  pub length: u32,
}

impl TextSpan {
  pub fn to_range(&self, line_index: &LineIndex) -> lsp::Range {
    lsp::Range {
      start: line_index.position_tsc(self.start.into()),
      end: line_index.position_tsc(TextSize::from(self.start + self.length)),
    }
  }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDisplayPart {
  text: String,
  kind: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JSDocTagInfo {
  name: String,
  text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickInfo {
  kind: ScriptElementKind,
  kind_modifiers: String,
  text_span: TextSpan,
  display_parts: Option<Vec<SymbolDisplayPart>>,
  documentation: Option<Vec<SymbolDisplayPart>>,
  tags: Option<Vec<JSDocTagInfo>>,
}

impl QuickInfo {
  pub fn to_hover(&self, line_index: &LineIndex) -> lsp::Hover {
    let mut contents = Vec::<lsp::MarkedString>::new();
    if let Some(display_string) =
      self.display_parts.clone().map(display_parts_to_string)
    {
      contents.push(lsp::MarkedString::from_language_code(
        "typescript".to_string(),
        display_string,
      ));
    }
    if let Some(documentation) =
      self.documentation.clone().map(display_parts_to_string)
    {
      contents.push(lsp::MarkedString::from_markdown(documentation));
    }
    if let Some(tags) = &self.tags {
      let tags_preview = tags
        .iter()
        .map(get_tag_documentation)
        .collect::<Vec<String>>()
        .join("  \n\n");
      if !tags_preview.is_empty() {
        contents.push(lsp::MarkedString::from_markdown(format!(
          "\n\n{}",
          tags_preview
        )));
      }
    }
    lsp::Hover {
      contents: lsp::HoverContents::Array(contents),
      range: Some(self.text_span.to_range(line_index)),
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSpan {
  text_span: TextSpan,
  pub file_name: String,
  original_text_span: Option<TextSpan>,
  original_file_name: Option<String>,
  context_span: Option<TextSpan>,
  original_context_span: Option<TextSpan>,
}

impl DocumentSpan {
  pub(crate) async fn to_link(
    &self,
    line_index: &LineIndex,
    language_server: &mut language_server::Inner,
  ) -> Option<lsp::LocationLink> {
    let target_specifier = resolve_url(&self.file_name).unwrap();
    let target_line_index = language_server
      .get_line_index(target_specifier.clone())
      .await
      .ok()?;
    let target_uri = language_server
      .url_map
      .normalize_specifier(&target_specifier)
      .unwrap();
    let (target_range, target_selection_range) =
      if let Some(context_span) = &self.context_span {
        (
          context_span.to_range(&target_line_index),
          self.text_span.to_range(&target_line_index),
        )
      } else {
        (
          self.text_span.to_range(&target_line_index),
          self.text_span.to_range(&target_line_index),
        )
      };
    let origin_selection_range =
      if let Some(original_context_span) = &self.original_context_span {
        Some(original_context_span.to_range(line_index))
      } else if let Some(original_text_span) = &self.original_text_span {
        Some(original_text_span.to_range(line_index))
      } else {
        None
      };
    let link = lsp::LocationLink {
      origin_selection_range,
      target_uri,
      target_range,
      target_selection_range,
    };
    Some(link)
  }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationTree {
  pub text: String,
  pub kind: ScriptElementKind,
  pub kind_modifiers: String,
  pub spans: Vec<TextSpan>,
  pub name_span: Option<TextSpan>,
  pub child_items: Option<Vec<NavigationTree>>,
}

impl NavigationTree {
  pub fn to_code_lens(
    &self,
    line_index: &LineIndex,
    specifier: &ModuleSpecifier,
    source: &CodeLensSource,
  ) -> lsp::CodeLens {
    let range = if let Some(name_span) = &self.name_span {
      name_span.to_range(line_index)
    } else if !self.spans.is_empty() {
      let span = &self.spans[0];
      span.to_range(line_index)
    } else {
      lsp::Range::default()
    };
    lsp::CodeLens {
      range,
      command: None,
      data: Some(json!({
        "specifier": specifier,
        "source": source
      })),
    }
  }

  pub fn walk<F>(&self, callback: &F)
  where
    F: Fn(&NavigationTree, Option<&NavigationTree>),
  {
    callback(self, None);
    if let Some(child_items) = &self.child_items {
      for child in child_items {
        child.walk_child(callback, self);
      }
    }
  }

  fn walk_child<F>(&self, callback: &F, parent: &NavigationTree)
  where
    F: Fn(&NavigationTree, Option<&NavigationTree>),
  {
    callback(self, Some(parent));
    if let Some(child_items) = &self.child_items {
      for child in child_items {
        child.walk_child(callback, self);
      }
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementationLocation {
  #[serde(flatten)]
  pub document_span: DocumentSpan,
  // ImplementationLocation props
  kind: ScriptElementKind,
  display_parts: Vec<SymbolDisplayPart>,
}

impl ImplementationLocation {
  pub(crate) fn to_location(
    &self,
    line_index: &LineIndex,
    language_server: &mut language_server::Inner,
  ) -> lsp::Location {
    let specifier = resolve_url(&self.document_span.file_name).unwrap();
    let uri = language_server
      .url_map
      .normalize_specifier(&specifier)
      .unwrap();
    lsp::Location {
      uri,
      range: self.document_span.text_span.to_range(line_index),
    }
  }

  pub(crate) async fn to_link(
    &self,
    line_index: &LineIndex,
    language_server: &mut language_server::Inner,
  ) -> Option<lsp::LocationLink> {
    self
      .document_span
      .to_link(line_index, language_server)
      .await
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameLocation {
  #[serde(flatten)]
  document_span: DocumentSpan,
  // RenameLocation props
  prefix_text: Option<String>,
  suffix_text: Option<String>,
}

pub struct RenameLocations {
  pub locations: Vec<RenameLocation>,
}

impl RenameLocations {
  pub(crate) async fn into_workspace_edit(
    self,
    new_name: &str,
    language_server: &mut language_server::Inner,
  ) -> Result<lsp::WorkspaceEdit, AnyError> {
    let mut text_document_edit_map: HashMap<Url, lsp::TextDocumentEdit> =
      HashMap::new();
    for location in self.locations.iter() {
      let specifier = resolve_url(&location.document_span.file_name)?;
      let uri = language_server.url_map.normalize_specifier(&specifier)?;

      // ensure TextDocumentEdit for `location.file_name`.
      if text_document_edit_map.get(&uri).is_none() {
        text_document_edit_map.insert(
          uri.clone(),
          lsp::TextDocumentEdit {
            text_document: lsp::OptionalVersionedTextDocumentIdentifier {
              uri: uri.clone(),
              version: language_server.document_version(specifier.clone()),
            },
            edits:
              Vec::<lsp::OneOf<lsp::TextEdit, lsp::AnnotatedTextEdit>>::new(),
          },
        );
      }

      // push TextEdit for ensured `TextDocumentEdit.edits`.
      let document_edit = text_document_edit_map.get_mut(&uri).unwrap();
      document_edit.edits.push(lsp::OneOf::Left(lsp::TextEdit {
        range: location
          .document_span
          .text_span
          .to_range(&language_server.get_line_index(specifier.clone()).await?),
        new_text: new_name.to_string(),
      }));
    }

    Ok(lsp::WorkspaceEdit {
      change_annotations: None,
      changes: None,
      document_changes: Some(lsp::DocumentChanges::Edits(
        text_document_edit_map.values().cloned().collect(),
      )),
    })
  }
}

#[derive(Debug, Deserialize)]
pub enum HighlightSpanKind {
  #[serde(rename = "none")]
  None,
  #[serde(rename = "definition")]
  Definition,
  #[serde(rename = "reference")]
  Reference,
  #[serde(rename = "writtenReference")]
  WrittenReference,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HighlightSpan {
  file_name: Option<String>,
  is_in_string: Option<bool>,
  text_span: TextSpan,
  context_span: Option<TextSpan>,
  kind: HighlightSpanKind,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionInfo {
  kind: ScriptElementKind,
  name: String,
  container_kind: Option<ScriptElementKind>,
  container_name: Option<String>,

  #[serde(flatten)]
  pub document_span: DocumentSpan,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionInfoAndBoundSpan {
  pub definitions: Option<Vec<DefinitionInfo>>,
  text_span: TextSpan,
}

impl DefinitionInfoAndBoundSpan {
  pub(crate) async fn to_definition(
    &self,
    line_index: &LineIndex,
    language_server: &mut language_server::Inner,
  ) -> Option<lsp::GotoDefinitionResponse> {
    if let Some(definitions) = &self.definitions {
      let mut location_links = Vec::<lsp::LocationLink>::new();
      for di in definitions {
        if let Some(link) =
          di.document_span.to_link(line_index, language_server).await
        {
          location_links.push(link);
        }
      }
      Some(lsp::GotoDefinitionResponse::Link(location_links))
    } else {
      None
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentHighlights {
  file_name: String,
  highlight_spans: Vec<HighlightSpan>,
}

impl DocumentHighlights {
  pub fn to_highlight(
    &self,
    line_index: &LineIndex,
  ) -> Vec<lsp::DocumentHighlight> {
    self
      .highlight_spans
      .iter()
      .map(|hs| lsp::DocumentHighlight {
        range: hs.text_span.to_range(line_index),
        kind: match hs.kind {
          HighlightSpanKind::WrittenReference => {
            Some(lsp::DocumentHighlightKind::Write)
          }
          _ => Some(lsp::DocumentHighlightKind::Read),
        },
      })
      .collect()
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TextChange {
  span: TextSpan,
  new_text: String,
}

impl TextChange {
  pub fn as_text_edit(
    &self,
    line_index: &LineIndex,
  ) -> lsp::OneOf<lsp::TextEdit, lsp::AnnotatedTextEdit> {
    lsp::OneOf::Left(lsp::TextEdit {
      range: self.span.to_range(line_index),
      new_text: self.new_text.clone(),
    })
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileTextChanges {
  file_name: String,
  text_changes: Vec<TextChange>,
  #[serde(skip_serializing_if = "Option::is_none")]
  is_new_file: Option<bool>,
}

impl FileTextChanges {
  pub(crate) async fn to_text_document_edit(
    &self,
    language_server: &mut language_server::Inner,
  ) -> Result<lsp::TextDocumentEdit, AnyError> {
    let specifier = resolve_url(&self.file_name)?;
    let line_index = language_server.get_line_index(specifier.clone()).await?;
    let edits = self
      .text_changes
      .iter()
      .map(|tc| tc.as_text_edit(&line_index))
      .collect();
    Ok(lsp::TextDocumentEdit {
      text_document: lsp::OptionalVersionedTextDocumentIdentifier {
        uri: specifier.clone(),
        version: language_server.document_version(specifier),
      },
      edits,
    })
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodeFixAction {
  pub description: String,
  pub changes: Vec<FileTextChanges>,
  // These are opaque types that should just be passed back when applying the
  // action.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commands: Option<Vec<Value>>,
  pub fix_name: String,
  // It appears currently that all fixIds are strings, but the protocol
  // specifies an opaque type, the problem is that we need to use the id as a
  // hash key, and `Value` does not implement hash (and it could provide a false
  // positive depending on JSON whitespace, so we deserialize it but it might
  // break in the future)
  #[serde(skip_serializing_if = "Option::is_none")]
  pub fix_id: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub fix_all_description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CombinedCodeActions {
  pub changes: Vec<FileTextChanges>,
  pub commands: Option<Vec<Value>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceEntry {
  is_write_access: bool,
  pub is_definition: bool,
  is_in_string: Option<bool>,
  #[serde(flatten)]
  pub document_span: DocumentSpan,
}

impl ReferenceEntry {
  pub(crate) fn to_location(
    &self,
    line_index: &LineIndex,
    language_server: &mut language_server::Inner,
  ) -> lsp::Location {
    let specifier = resolve_url(&self.document_span.file_name).unwrap();
    let uri = language_server
      .url_map
      .normalize_specifier(&specifier)
      .unwrap();
    lsp::Location {
      uri,
      range: self.document_span.text_span.to_range(line_index),
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionInfo {
  entries: Vec<CompletionEntry>,
  is_member_completion: bool,
}

impl CompletionInfo {
  pub fn into_completion_response(
    self,
    line_index: &LineIndex,
  ) -> lsp::CompletionResponse {
    let items = self
      .entries
      .into_iter()
      .map(|entry| entry.into_completion_item(line_index))
      .collect();
    lsp::CompletionResponse::Array(items)
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionEntry {
  kind: ScriptElementKind,
  kind_modifiers: Option<String>,
  name: String,
  sort_text: String,
  insert_text: Option<String>,
  replacement_span: Option<TextSpan>,
  has_action: Option<bool>,
  source: Option<String>,
  is_recommended: Option<bool>,
}

impl CompletionEntry {
  pub fn into_completion_item(
    self,
    line_index: &LineIndex,
  ) -> lsp::CompletionItem {
    let mut item = lsp::CompletionItem {
      label: self.name,
      kind: Some(self.kind.into()),
      sort_text: Some(self.sort_text.clone()),
      // TODO(lucacasonato): missing commit_characters
      ..Default::default()
    };

    if let Some(true) = self.is_recommended {
      // Make sure isRecommended property always comes first
      // https://github.com/Microsoft/vscode/issues/40325
      item.preselect = Some(true);
    } else if self.source.is_some() {
      // De-prioritze auto-imports
      // https://github.com/Microsoft/vscode/issues/40311
      item.sort_text = Some("\u{ffff}".to_string() + &self.sort_text)
    }

    match item.kind {
      Some(lsp::CompletionItemKind::Function)
      | Some(lsp::CompletionItemKind::Method) => {
        item.insert_text_format = Some(lsp::InsertTextFormat::Snippet);
      }
      _ => {}
    }

    let mut insert_text = self.insert_text;
    let replacement_range: Option<lsp::Range> =
      self.replacement_span.map(|span| span.to_range(line_index));

    // TODO(lucacasonato): port other special cases from https://github.com/theia-ide/typescript-language-server/blob/fdf28313833cd6216d00eb4e04dc7f00f4c04f09/server/src/completion.ts#L49-L55

    if let Some(kind_modifiers) = self.kind_modifiers {
      if kind_modifiers.contains("\\optional\\") {
        if insert_text.is_none() {
          insert_text = Some(item.label.clone());
        }
        if item.filter_text.is_none() {
          item.filter_text = Some(item.label.clone());
        }
        item.label += "?";
      }
    }

    if let Some(insert_text) = insert_text {
      if let Some(replacement_range) = replacement_range {
        item.text_edit = Some(lsp::CompletionTextEdit::Edit(
          lsp::TextEdit::new(replacement_range, insert_text),
        ));
      } else {
        item.insert_text = Some(insert_text);
      }
    }

    item
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpItems {
  items: Vec<SignatureHelpItem>,
  applicable_span: TextSpan,
  selected_item_index: u32,
  argument_index: u32,
  argument_count: u32,
}

impl SignatureHelpItems {
  pub fn into_signature_help(self) -> lsp::SignatureHelp {
    lsp::SignatureHelp {
      signatures: self
        .items
        .into_iter()
        .map(|item| item.into_signature_information())
        .collect(),
      active_parameter: Some(self.argument_index),
      active_signature: Some(self.selected_item_index),
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpItem {
  is_variadic: bool,
  prefix_display_parts: Vec<SymbolDisplayPart>,
  suffix_display_parts: Vec<SymbolDisplayPart>,
  separator_display_parts: Vec<SymbolDisplayPart>,
  parameters: Vec<SignatureHelpParameter>,
  documentation: Vec<SymbolDisplayPart>,
  tags: Vec<JSDocTagInfo>,
}

impl SignatureHelpItem {
  pub fn into_signature_information(self) -> lsp::SignatureInformation {
    let prefix_text = display_parts_to_string(self.prefix_display_parts);
    let params_text = self
      .parameters
      .iter()
      .map(|param| display_parts_to_string(param.display_parts.clone()))
      .collect::<Vec<String>>()
      .join(", ");
    let suffix_text = display_parts_to_string(self.suffix_display_parts);
    lsp::SignatureInformation {
      label: format!("{}{}{}", prefix_text, params_text, suffix_text),
      documentation: Some(lsp::Documentation::String(display_parts_to_string(
        self.documentation,
      ))),
      parameters: Some(
        self
          .parameters
          .into_iter()
          .map(|param| param.into_parameter_information())
          .collect(),
      ),
      active_parameter: None,
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpParameter {
  name: String,
  documentation: Vec<SymbolDisplayPart>,
  display_parts: Vec<SymbolDisplayPart>,
  is_optional: bool,
}

impl SignatureHelpParameter {
  pub fn into_parameter_information(self) -> lsp::ParameterInformation {
    lsp::ParameterInformation {
      label: lsp::ParameterLabel::Simple(display_parts_to_string(
        self.display_parts,
      )),
      documentation: Some(lsp::Documentation::String(display_parts_to_string(
        self.documentation,
      ))),
    }
  }
}

#[derive(Debug, Clone, Deserialize)]
struct Response {
  id: usize,
  data: Value,
}

struct State<'a> {
  asset: Option<String>,
  last_id: usize,
  response: Option<Response>,
  state_snapshot: StateSnapshot,
  snapshots: HashMap<(Cow<'a, str>, Cow<'a, str>), String>,
}

impl<'a> State<'a> {
  fn new(state_snapshot: StateSnapshot) -> Self {
    Self {
      asset: None,
      last_id: 1,
      response: None,
      state_snapshot,
      snapshots: Default::default(),
    }
  }
}

/// If a snapshot is missing from the state cache, add it.
fn cache_snapshot(
  state: &mut State,
  specifier: String,
  version: String,
) -> Result<(), AnyError> {
  if !state
    .snapshots
    .contains_key(&(specifier.clone().into(), version.clone().into()))
  {
    let s = resolve_url(&specifier)?;
    let content = state.state_snapshot.documents.content(&s)?.unwrap();
    state
      .snapshots
      .insert((specifier.into(), version.into()), content);
  }
  Ok(())
}

fn op<F>(op_fn: F) -> Box<OpFn>
where
  F: Fn(&mut State, Value) -> Result<Value, AnyError> + 'static,
{
  json_op_sync(move |s, args, _bufs| {
    let state = s.borrow_mut::<State>();
    op_fn(state, args)
  })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceSnapshotArgs {
  specifier: String,
  version: String,
}

/// The language service is dropping a reference to a source file snapshot, and
/// we can drop our version of that document.
fn dispose(state: &mut State, args: Value) -> Result<Value, AnyError> {
  let mark = state.state_snapshot.performance.mark("op_dispose");
  let v: SourceSnapshotArgs = serde_json::from_value(args)?;
  state
    .snapshots
    .remove(&(v.specifier.into(), v.version.into()));
  state.state_snapshot.performance.measure(mark);
  Ok(json!(true))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetChangeRangeArgs {
  specifier: String,
  old_length: u32,
  old_version: String,
  version: String,
}

/// The language service wants to compare an old snapshot with a new snapshot to
/// determine what source hash changed.
fn get_change_range(state: &mut State, args: Value) -> Result<Value, AnyError> {
  let mark = state.state_snapshot.performance.mark("op_get_change_range");
  let v: GetChangeRangeArgs = serde_json::from_value(args.clone())?;
  cache_snapshot(state, v.specifier.clone(), v.version.clone())?;
  if let Some(current) = state
    .snapshots
    .get(&(v.specifier.clone().into(), v.version.into()))
  {
    if let Some(prev) = state
      .snapshots
      .get(&(v.specifier.clone().into(), v.old_version.clone().into()))
    {
      state.state_snapshot.performance.measure(mark);
      Ok(text::get_range_change(prev, current))
    } else {
      let new_length = current.encode_utf16().count();
      state.state_snapshot.performance.measure(mark);
      // when a local file is opened up in the editor, the compiler might
      // already have a snapshot of it in memory, and will request it, but we
      // now are working off in memory versions of the document, and so need
      // to tell tsc to reset the whole document
      Ok(json!({
        "span": {
          "start": 0,
          "length": v.old_length,
        },
        "newLength": new_length,
      }))
    }
  } else {
    state.state_snapshot.performance.measure(mark);
    Err(custom_error(
      "MissingSnapshot",
      format!(
        "The current snapshot version is missing.\n  Args: \"{}\"",
        args
      ),
    ))
  }
}

fn get_length(state: &mut State, args: Value) -> Result<Value, AnyError> {
  let mark = state.state_snapshot.performance.mark("op_get_length");
  let v: SourceSnapshotArgs = serde_json::from_value(args)?;
  let specifier = resolve_url(&v.specifier)?;
  if let Some(Some(asset)) = state.state_snapshot.assets.get(&specifier) {
    Ok(json!(asset.length))
  } else if state.state_snapshot.documents.contains_key(&specifier) {
    cache_snapshot(state, v.specifier.clone(), v.version.clone())?;
    let content = state
      .snapshots
      .get(&(v.specifier.into(), v.version.into()))
      .unwrap();
    state.state_snapshot.performance.measure(mark);
    Ok(json!(content.encode_utf16().count()))
  } else {
    let sources = &mut state.state_snapshot.sources;
    state.state_snapshot.performance.measure(mark);
    Ok(json!(sources.get_length_utf16(&specifier).unwrap()))
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetTextArgs {
  specifier: String,
  version: String,
  start: usize,
  end: usize,
}

fn get_text(state: &mut State, args: Value) -> Result<Value, AnyError> {
  let mark = state.state_snapshot.performance.mark("op_get_text");
  let v: GetTextArgs = serde_json::from_value(args)?;
  let specifier = resolve_url(&v.specifier)?;
  let content =
    if let Some(Some(content)) = state.state_snapshot.assets.get(&specifier) {
      content.text.clone()
    } else if state.state_snapshot.documents.contains_key(&specifier) {
      cache_snapshot(state, v.specifier.clone(), v.version.clone())?;
      state
        .snapshots
        .get(&(v.specifier.into(), v.version.into()))
        .unwrap()
        .clone()
    } else {
      state.state_snapshot.sources.get_source(&specifier).unwrap()
    };
  state.state_snapshot.performance.measure(mark);
  Ok(json!(text::slice(&content, v.start..v.end)))
}

fn resolve(state: &mut State, args: Value) -> Result<Value, AnyError> {
  let mark = state.state_snapshot.performance.mark("op_resolve");
  let v: ResolveArgs = serde_json::from_value(args)?;
  let mut resolved = Vec::<Option<(String, String)>>::new();
  let referrer = resolve_url(&v.base)?;
  let sources = &mut state.state_snapshot.sources;

  if state.state_snapshot.documents.contains_key(&referrer) {
    if let Some(dependencies) =
      state.state_snapshot.documents.dependencies(&referrer)
    {
      for specifier in &v.specifiers {
        if specifier.starts_with("asset:///") {
          resolved.push(Some((
            specifier.clone(),
            MediaType::from(specifier).as_ts_extension().into(),
          )))
        } else if let Some(dependency) = dependencies.get(specifier) {
          let resolved_import =
            if let Some(resolved_import) = &dependency.maybe_type {
              resolved_import.clone()
            } else if let Some(resolved_import) = &dependency.maybe_code {
              resolved_import.clone()
            } else {
              ResolvedDependency::Err(ResolvedDependencyErr::Missing)
            };
          if let ResolvedDependency::Resolved(resolved_specifier) =
            resolved_import
          {
            if state
              .state_snapshot
              .documents
              .contains_key(&resolved_specifier)
            {
              let media_type = MediaType::from(&resolved_specifier);
              resolved.push(Some((
                resolved_specifier.to_string(),
                media_type.as_ts_extension().into(),
              )));
            } else if sources.contains_key(&resolved_specifier) {
              let media_type = if let Some(media_type) =
                sources.get_media_type(&resolved_specifier)
              {
                media_type
              } else {
                MediaType::from(&resolved_specifier)
              };
              resolved.push(Some((
                resolved_specifier.to_string(),
                media_type.as_ts_extension().into(),
              )));
            } else {
              resolved.push(None);
            }
          } else {
            resolved.push(None);
          }
        }
      }
    }
  } else if sources.contains_key(&referrer) {
    for specifier in &v.specifiers {
      if let Some((resolved_specifier, media_type)) =
        sources.resolve_import(specifier, &referrer)
      {
        resolved.push(Some((
          resolved_specifier.to_string(),
          media_type.as_ts_extension().into(),
        )));
      } else {
        resolved.push(None);
      }
    }
  } else {
    state.state_snapshot.performance.measure(mark);
    return Err(custom_error(
      "NotFound",
      format!(
        "the referring ({}) specifier is unexpectedly missing",
        referrer
      ),
    ));
  }

  state.state_snapshot.performance.measure(mark);
  Ok(json!(resolved))
}

fn respond(state: &mut State, args: Value) -> Result<Value, AnyError> {
  state.response = Some(serde_json::from_value(args)?);
  Ok(json!(true))
}

#[allow(clippy::unnecessary_wraps)]
fn script_names(state: &mut State, _args: Value) -> Result<Value, AnyError> {
  Ok(json!(state.state_snapshot.documents.open_specifiers()))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScriptVersionArgs {
  specifier: String,
}

fn script_version(state: &mut State, args: Value) -> Result<Value, AnyError> {
  let mark = state.state_snapshot.performance.mark("op_script_version");
  let v: ScriptVersionArgs = serde_json::from_value(args)?;
  let specifier = resolve_url(&v.specifier)?;
  if specifier.scheme() == "asset" {
    return if state.state_snapshot.assets.contains_key(&specifier) {
      Ok(json!("1"))
    } else {
      Ok(json!(null))
    };
  } else if let Some(version) =
    state.state_snapshot.documents.version(&specifier)
  {
    return Ok(json!(version.to_string()));
  } else {
    let sources = &mut state.state_snapshot.sources;
    if let Some(version) = sources.get_script_version(&specifier) {
      return Ok(json!(version));
    }
  }

  state.state_snapshot.performance.measure(mark);
  Ok(json!(null))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetAssetArgs {
  text: Option<String>,
}

fn set_asset(state: &mut State, args: Value) -> Result<Value, AnyError> {
  let v: SetAssetArgs = serde_json::from_value(args)?;
  state.asset = v.text;
  Ok(json!(true))
}

/// Create and setup a JsRuntime based on a snapshot. It is expected that the
/// supplied snapshot is an isolate that contains the TypeScript language
/// server.
pub fn start(debug: bool) -> Result<JsRuntime, AnyError> {
  let mut runtime = JsRuntime::new(RuntimeOptions {
    startup_snapshot: Some(tsc::compiler_snapshot()),
    ..Default::default()
  });

  {
    let op_state = runtime.op_state();
    let mut op_state = op_state.borrow_mut();
    op_state.put(State::new(StateSnapshot::default()));
  }

  runtime.register_op("op_dispose", op(dispose));
  runtime.register_op("op_get_change_range", op(get_change_range));
  runtime.register_op("op_get_length", op(get_length));
  runtime.register_op("op_get_text", op(get_text));
  runtime.register_op("op_resolve", op(resolve));
  runtime.register_op("op_respond", op(respond));
  runtime.register_op("op_script_names", op(script_names));
  runtime.register_op("op_script_version", op(script_version));
  runtime.register_op("op_set_asset", op(set_asset));

  let init_config = json!({ "debug": debug });
  let init_src = format!("globalThis.serverInit({});", init_config);

  runtime.execute("[native code]", &init_src)?;
  Ok(runtime)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum QuotePreference {
  Auto,
  Double,
  Single,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum ImportModuleSpecifierPreference {
  Auto,
  Relative,
  NonRelative,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum ImportModuleSpecifierEnding {
  Auto,
  Minimal,
  Index,
  Js,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum IncludePackageJsonAutoImports {
  Auto,
  On,
  Off,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPreferences {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub disable_suggestions: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub quote_preference: Option<QuotePreference>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_for_module_exports: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_automatic_optional_chain_completions: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_with_insert_text: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub import_module_specifier_preference:
    Option<ImportModuleSpecifierPreference>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub import_module_specifier_ending: Option<ImportModuleSpecifierEnding>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub allow_text_changes_in_new_files: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub provide_prefix_and_suffix_text_for_rename: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_package_json_auto_imports: Option<IncludePackageJsonAutoImports>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub provide_refactor_not_applicable_reason: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpItemsOptions {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub trigger_reason: Option<SignatureHelpTriggerReason>,
}

#[derive(Debug, Serialize)]
pub enum SignatureHelpTriggerKind {
  #[serde(rename = "characterTyped")]
  CharacterTyped,
  #[serde(rename = "invoked")]
  Invoked,
  #[serde(rename = "retrigger")]
  Retrigger,
}

impl From<lsp::SignatureHelpTriggerKind> for SignatureHelpTriggerKind {
  fn from(kind: lsp::SignatureHelpTriggerKind) -> Self {
    match kind {
      lsp::SignatureHelpTriggerKind::Invoked => Self::Invoked,
      lsp::SignatureHelpTriggerKind::TriggerCharacter => Self::CharacterTyped,
      lsp::SignatureHelpTriggerKind::ContentChange => Self::Retrigger,
    }
  }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpTriggerReason {
  pub kind: SignatureHelpTriggerKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub trigger_character: Option<String>,
}

/// Methods that are supported by the Language Service in the compiler isolate.
#[derive(Debug)]
pub enum RequestMethod {
  /// Configure the compilation settings for the server.
  Configure(TsConfig),
  /// Get rename locations at a given position.
  FindRenameLocations((ModuleSpecifier, u32, bool, bool, bool)),
  /// Retrieve the text of an assets that exists in memory in the isolate.
  GetAsset(ModuleSpecifier),
  /// Retrieve code fixes for a range of a file with the provided error codes.
  GetCodeFixes((ModuleSpecifier, u32, u32, Vec<String>)),
  /// Get completion information at a given position (IntelliSense).
  GetCompletions((ModuleSpecifier, u32, UserPreferences)),
  /// Retrieve the combined code fixes for a fix id for a module.
  GetCombinedCodeFix((ModuleSpecifier, Value)),
  /// Get declaration information for a specific position.
  GetDefinition((ModuleSpecifier, u32)),
  /// Return diagnostics for given file.
  GetDiagnostics(Vec<ModuleSpecifier>),
  /// Return document highlights at position.
  GetDocumentHighlights((ModuleSpecifier, u32, Vec<ModuleSpecifier>)),
  /// Get implementation information for a specific position.
  GetImplementation((ModuleSpecifier, u32)),
  /// Get a "navigation tree" for a specifier.
  GetNavigationTree(ModuleSpecifier),
  /// Return quick info at position (hover information).
  GetQuickInfo((ModuleSpecifier, u32)),
  /// Get document references for a specific position.
  GetReferences((ModuleSpecifier, u32)),
  /// Get signature help items for a specific position.
  GetSignatureHelpItems((ModuleSpecifier, u32, SignatureHelpItemsOptions)),
  /// Get the diagnostic codes that support some form of code fix.
  GetSupportedCodeFixes,
}

impl RequestMethod {
  pub fn to_value(&self, id: usize) -> Value {
    match self {
      RequestMethod::Configure(config) => json!({
        "id": id,
        "method": "configure",
        "compilerOptions": config,
      }),
      RequestMethod::FindRenameLocations((
        specifier,
        position,
        find_in_strings,
        find_in_comments,
        provide_prefix_and_suffix_text_for_rename,
      )) => {
        json!({
          "id": id,
          "method": "findRenameLocations",
          "specifier": specifier,
          "position": position,
          "findInStrings": find_in_strings,
          "findInComments": find_in_comments,
          "providePrefixAndSuffixTextForRename": provide_prefix_and_suffix_text_for_rename
        })
      }
      RequestMethod::GetAsset(specifier) => json!({
        "id": id,
        "method": "getAsset",
        "specifier": specifier,
      }),
      RequestMethod::GetCodeFixes((
        specifier,
        start_pos,
        end_pos,
        error_codes,
      )) => json!({
        "id": id,
        "method": "getCodeFixes",
        "specifier": specifier,
        "startPosition": start_pos,
        "endPosition": end_pos,
        "errorCodes": error_codes,
      }),
      RequestMethod::GetCombinedCodeFix((specifier, fix_id)) => json!({
        "id": id,
        "method": "getCombinedCodeFix",
        "specifier": specifier,
        "fixId": fix_id,
      }),
      RequestMethod::GetCompletions((specifier, position, preferences)) => {
        json!({
          "id": id,
          "method": "getCompletions",
          "specifier": specifier,
          "position": position,
          "preferences": preferences,
        })
      }
      RequestMethod::GetDefinition((specifier, position)) => json!({
        "id": id,
        "method": "getDefinition",
        "specifier": specifier,
        "position": position,
      }),
      RequestMethod::GetDiagnostics(specifiers) => json!({
        "id": id,
        "method": "getDiagnostics",
        "specifiers": specifiers,
      }),
      RequestMethod::GetDocumentHighlights((
        specifier,
        position,
        files_to_search,
      )) => json!({
        "id": id,
        "method": "getDocumentHighlights",
        "specifier": specifier,
        "position": position,
        "filesToSearch": files_to_search,
      }),
      RequestMethod::GetImplementation((specifier, position)) => json!({
        "id": id,
        "method": "getImplementation",
        "specifier": specifier,
        "position": position,
      }),
      RequestMethod::GetNavigationTree(specifier) => json!({
        "id": id,
        "method": "getNavigationTree",
        "specifier": specifier,
      }),
      RequestMethod::GetQuickInfo((specifier, position)) => json!({
        "id": id,
        "method": "getQuickInfo",
        "specifier": specifier,
        "position": position,
      }),
      RequestMethod::GetReferences((specifier, position)) => json!({
        "id": id,
        "method": "getReferences",
        "specifier": specifier,
        "position": position,
      }),
      RequestMethod::GetSignatureHelpItems((specifier, position, options)) => {
        json!({
          "id": id,
          "method": "getSignatureHelpItems",
          "specifier": specifier,
          "position": position,
          "options": options,
        })
      }
      RequestMethod::GetSupportedCodeFixes => json!({
        "id": id,
        "method": "getSupportedCodeFixes",
      }),
    }
  }
}

/// Send a request into a runtime and return the JSON value of the response.
pub fn request(
  runtime: &mut JsRuntime,
  state_snapshot: StateSnapshot,
  method: RequestMethod,
) -> Result<Value, AnyError> {
  let id = {
    let op_state = runtime.op_state();
    let mut op_state = op_state.borrow_mut();
    let state = op_state.borrow_mut::<State>();
    state.state_snapshot = state_snapshot;
    state.last_id += 1;
    state.last_id
  };
  let request_params = method.to_value(id);
  let request_src = format!("globalThis.serverRequest({});", request_params);
  runtime.execute("[native_code]", &request_src)?;

  let op_state = runtime.op_state();
  let mut op_state = op_state.borrow_mut();
  let state = op_state.borrow_mut::<State>();

  if let Some(response) = state.response.clone() {
    state.response = None;
    Ok(response.data)
  } else {
    Err(custom_error(
      "RequestError",
      "The response was not received for the request.",
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::lsp::documents::DocumentCache;

  fn mock_state_snapshot(sources: Vec<(&str, &str, i32)>) -> StateSnapshot {
    let mut documents = DocumentCache::default();
    for (specifier, content, version) in sources {
      let specifier =
        resolve_url(specifier).expect("failed to create specifier");
      documents.open(specifier, version, content);
    }
    StateSnapshot {
      documents,
      ..Default::default()
    }
  }

  fn setup(
    debug: bool,
    config: Value,
    sources: Vec<(&str, &str, i32)>,
  ) -> (JsRuntime, StateSnapshot) {
    let state_snapshot = mock_state_snapshot(sources.clone());
    let mut runtime = start(debug).expect("could not start server");
    let ts_config = TsConfig::new(config);
    assert_eq!(
      request(
        &mut runtime,
        state_snapshot.clone(),
        RequestMethod::Configure(ts_config)
      )
      .expect("failed request"),
      json!(true)
    );
    (runtime, state_snapshot)
  }

  #[test]
  fn test_replace_links() {
    let actual = replace_links(r"test {@link http://deno.land/x/mod.ts} test");
    assert_eq!(
      actual,
      r"test [http://deno.land/x/mod.ts](http://deno.land/x/mod.ts) test"
    );
    let actual =
      replace_links(r"test {@link http://deno.land/x/mod.ts a link} test");
    assert_eq!(actual, r"test [a link](http://deno.land/x/mod.ts) test");
    let actual =
      replace_links(r"test {@linkcode http://deno.land/x/mod.ts a link} test");
    assert_eq!(actual, r"test [`a link`](http://deno.land/x/mod.ts) test");
  }

  #[test]
  fn test_project_configure() {
    setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "noEmit": true,
      }),
      vec![],
    );
  }

  #[test]
  fn test_project_reconfigure() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "noEmit": true,
      }),
      vec![],
    );
    let ts_config = TsConfig::new(json!({
      "target": "esnext",
      "module": "esnext",
      "noEmit": true,
      "lib": ["deno.ns", "deno.worker"]
    }));
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::Configure(ts_config),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response, json!(true));
  }

  #[test]
  fn test_get_diagnostics() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "noEmit": true,
      }),
      vec![("file:///a.ts", r#"console.log("hello deno");"#, 1)],
    );
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetDiagnostics(vec![specifier]),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(
      response,
      json!({
        "file:///a.ts": [
          {
            "start": {
              "line": 0,
              "character": 0,
            },
            "end": {
              "line": 0,
              "character": 7
            },
            "fileName": "file:///a.ts",
            "messageText": "Cannot find name 'console'. Do you need to change your target library? Try changing the `lib` compiler option to include 'dom'.",
            "sourceLine": "console.log(\"hello deno\");",
            "category": 1,
            "code": 2584
          }
        ]
      })
    );
  }

  #[test]
  fn test_get_diagnostics_lib() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "jsx": "react",
        "lib": ["esnext", "dom", "deno.ns"],
        "noEmit": true,
      }),
      vec![("file:///a.ts", r#"console.log(document.location);"#, 1)],
    );
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetDiagnostics(vec![specifier]),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response, json!({ "file:///a.ts": [] }));
  }

  #[test]
  fn test_module_resolution() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      vec![(
        "file:///a.ts",
        r#"
        import { B } from "https://deno.land/x/b/mod.ts";

        const b = new B();

        console.log(b);
      "#,
        1,
      )],
    );
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetDiagnostics(vec![specifier]),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response, json!({ "file:///a.ts": [] }));
  }

  #[test]
  fn test_bad_module_specifiers() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      vec![(
        "file:///a.ts",
        r#"
        import { A } from ".";
        "#,
        1,
      )],
    );
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetDiagnostics(vec![specifier]),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(
      response,
      json!({
        "file:///a.ts": [{
          "start": {
            "line": 1,
            "character": 8
          },
          "end": {
            "line": 1,
            "character": 30
          },
          "fileName": "file:///a.ts",
          "messageText": "\'A\' is declared but its value is never read.",
          "sourceLine": "        import { A } from \".\";",
          "category": 2,
          "code": 6133,
          "reportsUnnecessary": true,
        }]
      })
    );
  }

  #[test]
  fn test_remote_modules() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      vec![(
        "file:///a.ts",
        r#"
        import { B } from "https://deno.land/x/b/mod.ts";

        const b = new B();

        console.log(b);
      "#,
        1,
      )],
    );
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetDiagnostics(vec![specifier]),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response, json!({ "file:///a.ts": [] }));
  }

  #[test]
  fn test_partial_modules() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      vec![(
        "file:///a.ts",
        r#"
        import {
          Application,
          Context,
          Router,
          Status,
        } from "https://deno.land/x/oak@v6.3.2/mod.ts";

        import * as test from
      "#,
        1,
      )],
    );
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetDiagnostics(vec![specifier]),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(
      response,
      json!({
        "file:///a.ts": [{
          "start": {
            "line": 1,
            "character": 8
          },
          "end": {
            "line": 6,
            "character": 55,
          },
          "fileName": "file:///a.ts",
          "messageText": "All imports in import declaration are unused.",
          "sourceLine": "        import {",
          "category": 2,
          "code": 6192,
          "reportsUnnecessary": true
        }, {
          "start": {
            "line": 8,
            "character": 29
          },
          "end": {
            "line": 8,
            "character": 29
          },
          "fileName": "file:///a.ts",
          "messageText": "Expression expected.",
          "sourceLine": "        import * as test from",
          "category": 1,
          "code": 1109
        }]
      })
    );
  }

  #[test]
  fn test_no_debug_failure() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      vec![("file:///a.ts", r#"const url = new URL("b.js", import."#, 1)],
    );
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetDiagnostics(vec![specifier]),
    );
    assert!(result.is_ok());
    let response = result.unwrap();
    assert_eq!(response, json!({}));
  }

  #[test]
  fn test_request_asset() {
    let (mut runtime, state_snapshot) = setup(
      false,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      vec![],
    );
    let specifier =
      resolve_url("asset:///lib.esnext.d.ts").expect("could not resolve url");
    let result = request(
      &mut runtime,
      state_snapshot,
      RequestMethod::GetAsset(specifier),
    );
    assert!(result.is_ok());
    let response: Option<String> =
      serde_json::from_value(result.unwrap()).unwrap();
    assert!(response.is_some());
  }
}
