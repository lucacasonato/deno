// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use std::env;

fn main() {
  println!("cargo:rustc-env=TARGET={}", env::var("TARGET").unwrap());
  
  // This is a hack to ensure that cargo does not rebuild this crate just
  // because a JS file has changed.
  println!("cargo:rerun-if-changed=build.rs");
}
