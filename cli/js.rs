// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

pub const DENO_RT_SNAPSHOT_SECTION_NAME: &str = "deno_rt_snapshot";

pub fn deno_isolate_init() -> Option<&'static [u8]> {
  let cli_snapshot = libsui::find_section(DENO_RT_SNAPSHOT_SECTION_NAME);
  if cli_snapshot.is_none() && std::env::var("DENO_FINALIZE_BUILD").is_err() {
    panic!("The deno binary is not finalized. Run `DENO_FINALIZE_BUILD=1 deno` to finalize the binary.");
  }
  cli_snapshot
}
