// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

use deno_core::Extension;

/// Load and execute the javascript code.
pub fn init() -> Extension {
  Extension::builder()
    .js(include_js_files!(
      prefix "deno:ext/web",
      "00_kv.js",
    ))
    .ops(vec![])
    .build()
}
