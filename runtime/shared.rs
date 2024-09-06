// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.
// Utilities shared between `build.rs` and the rest of the crate.

use deno_ast::MediaType;
use deno_ast::ParseParams;
use deno_ast::SourceMapOption;
use deno_core::error::AnyError;
use deno_core::extension;
use deno_core::Extension;
use deno_core::ModuleCodeString;
use deno_core::ModuleName;
use deno_core::SourceMapData;
use deno_crypto::rand;
use std::env::temp_dir;
use std::hash::Hash;
use std::hash::Hasher as _;
use std::path::Path;

extension!(runtime,
  deps = [
    deno_webidl,
    deno_console,
    deno_url,
    deno_tls,
    deno_web,
    deno_fetch,
    deno_cache,
    deno_websocket,
    deno_webstorage,
    deno_crypto,
    deno_broadcast_channel,
    deno_node,
    deno_ffi,
    deno_net,
    deno_napi,
    deno_http,
    deno_io,
    deno_fs
  ],
  esm_entry_point = "ext:runtime/90_deno_ns.js",
  esm = [
    dir "js",
    "01_errors.js",
    "01_version.ts",
    "06_util.js",
    "10_permissions.js",
    "11_workers.js",
    "13_buffer.js",
    "30_os.js",
    "40_fs_events.js",
    "40_process.js",
    "40_signals.js",
    "40_tty.js",
    "41_prompt.js",
    "90_deno_ns.js",
    "98_global_scope_shared.js",
    "98_global_scope_window.js",
    "98_global_scope_worker.js"
  ],
  customizer = |ext: &mut Extension| {
    #[cfg(not(feature = "exclude_runtime_main_js"))]
    {
      use deno_core::ascii_str_include;
      use deno_core::ExtensionFileSource;
      ext.esm_files.to_mut().push(ExtensionFileSource::new("ext:runtime_main/js/99_main.js", ascii_str_include!("./js/99_main.js")));
      ext.esm_entry_point = Some("ext:runtime_main/js/99_main.js");
    }
  }
);

pub fn maybe_transpile_source(
  name: ModuleName,
  source: ModuleCodeString,
) -> Result<(ModuleCodeString, Option<SourceMapData>), AnyError> {
  // Always transpile `node:` built-in modules, since they might be TypeScript.
  let media_type = if name.starts_with("node:") {
    MediaType::TypeScript
  } else {
    MediaType::from_path(Path::new(&name))
  };

  match media_type {
    MediaType::TypeScript => {}
    MediaType::JavaScript | MediaType::Mjs => return Ok((source, None)),
    _ => panic!(
      "Unsupported media type for snapshotting {media_type:?} for file {}",
      name
    ),
  }

  let transpile_options = deno_ast::TranspileOptions {
    imports_not_used_as_values: deno_ast::ImportsNotUsedAsValues::Remove,
    ..Default::default()
  };
  let emit_options = deno_ast::EmitOptions {
    source_map: if cfg!(debug_assertions) {
      SourceMapOption::Separate
    } else {
      SourceMapOption::None
    },
    ..Default::default()
  };

  let mut hasher = std::hash::DefaultHasher::new();
  source.hash(&mut hasher);
  transpile_options.hash(&mut hasher);
  emit_options.hash(&mut hasher);
  let source_hash = hasher.finish();

  let base_path = temp_dir().join("deno_rt_ext_transpile_cache");
  std::fs::create_dir_all(&base_path)?;
  let js_path = base_path.join(format!("{:x}.js", source_hash));
  let js_map_path = base_path.join(format!("{:x}.js.map", source_hash));

  let js_source = std::fs::read_to_string(&js_path).ok();

  'a: {
    if let Some(source_text) = js_source {
      let maybe_source_map =
        if emit_options.source_map == SourceMapOption::Separate {
          if let Some(source_map) = std::fs::read(&js_map_path).ok() {
            Some(source_map.into())
          } else {
            break 'a;
          }
        } else {
          None
        };
      return Ok((source_text.into(), maybe_source_map));
    }
  }

  let parsed = deno_ast::parse_module(ParseParams {
    specifier: deno_core::url::Url::parse(&name).unwrap(),
    text: source.into(),
    media_type,
    capture_tokens: false,
    scope_analysis: false,
    maybe_syntax: None,
  })?;
  let transpiled_source = parsed
    .transpile(
      &deno_ast::TranspileOptions {
        imports_not_used_as_values: deno_ast::ImportsNotUsedAsValues::Remove,
        ..Default::default()
      },
      &deno_ast::EmitOptions {
        source_map: if cfg!(debug_assertions) {
          SourceMapOption::Separate
        } else {
          SourceMapOption::None
        },
        ..Default::default()
      },
    )?
    .into_source();

  let maybe_source_map: Option<SourceMapData> =
    transpiled_source.source_map.map(|sm| sm.into());
  let source_text = String::from_utf8(transpiled_source.source)?;

  let random_suffix = rand::random::<u32>();
  let js_path_tmp =
    base_path.join(format!("{:x}-{}.js", source_hash, random_suffix));
  let js_map_path_tmp =
    base_path.join(format!("{:x}-{}.js.map", source_hash, random_suffix));
  std::fs::write(&js_path_tmp, &source_text)?;
  if let Some(source_map) = &maybe_source_map {
    std::fs::write(&js_map_path_tmp, &source_map)?;
  }
  std::fs::rename(&js_path_tmp, &js_path)?;
  if maybe_source_map.is_some() {
    std::fs::rename(&js_map_path_tmp, &js_map_path)?;
  }

  Ok((source_text.into(), maybe_source_map))
}
