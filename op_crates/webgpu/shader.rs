// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use deno_core::error::bad_resource_id;
use deno_core::error::AnyError;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::ResourceId;
use deno_core::ZeroCopyBuf;
use deno_core::{OpState, Resource};
use serde::Deserialize;
use std::borrow::Cow;
use std::rc::Rc;

use crate::Instance;

use super::error::WebGpuError;

pub(crate) struct WebGpuShaderModule {
  instance: Rc<Instance>,
  _device: Rc<wgpu_core::id::DeviceId>,
  pub shader_module: Rc<wgpu_core::id::ShaderModuleId>,
}
impl Resource for WebGpuShaderModule {
  fn name(&self) -> Cow<str> {
    "webGPUShaderModule".into()
  }

  fn close(self: Rc<Self>) {
    let resource = Rc::try_unwrap(self)
      .map_err(|_| "closed webGPUShaderModule while in use")
      .unwrap();
    let instance = resource.instance;
    let shader_module = Rc::try_unwrap(resource.shader_module)
      .map_err(|_| "closed webGPUShaderModule while it still had children")
      .unwrap();
    gfx_select!(shader_module => instance.shader_module_drop(shader_module));
  }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateShaderModuleArgs {
  device_rid: ResourceId,
  label: Option<String>,
  code: Option<String>,
  _source_map: Option<()>, // not yet implemented
}

pub fn op_webgpu_create_shader_module(
  state: &mut OpState,
  args: CreateShaderModuleArgs,
  zero_copy: &mut [ZeroCopyBuf],
) -> Result<Value, AnyError> {
  let device_resource = state
    .resource_table
    .get::<super::WebGpuDevice>(args.device_rid)
    .ok_or_else(bad_resource_id)?;
  let instance = device_resource.instance.clone();
  let device = device_resource.device.clone();

  let source = match args.code {
    Some(code) => {
      wgpu_core::pipeline::ShaderModuleSource::Wgsl(Cow::from(code))
    }
    None => wgpu_core::pipeline::ShaderModuleSource::SpirV(Cow::from(unsafe {
      let (prefix, data, suffix) = zero_copy[0].align_to::<u32>();
      assert!(prefix.is_empty());
      assert!(suffix.is_empty());
      data
    })),
  };

  let mut flags = wgpu_types::ShaderFlags::default();
  flags.set(wgpu_types::ShaderFlags::VALIDATION, true);
  #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
  flags.set(wgpu_types::ShaderFlags::EXPERIMENTAL_TRANSLATION, true);

  let descriptor = wgpu_core::pipeline::ShaderModuleDescriptor {
    label: args.label.map(Cow::from),
    flags,
  };

  let (shader_module, maybe_err) = gfx_select!(device => instance.device_create_shader_module(
    *device,
    &descriptor,
    source,
    std::marker::PhantomData
  ));

  let rid = state.resource_table.add(WebGpuShaderModule {
    instance,
    _device: device,
    shader_module: Rc::new(shader_module),
  });

  Ok(json!({
    "rid": rid,
    "err": maybe_err.map(WebGpuError::from)
  }))
}
