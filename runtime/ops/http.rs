use deno_core::error::bad_resource_id;
use deno_core::error::AnyError;
use deno_core::futures::Future;
use deno_core::futures::FutureExt;
use deno_core::serde::Deserialize;
use deno_core::serde_json;
use deno_core::serde_json::Value;
use deno_core::BufVec;
use deno_core::OpState;

use http::Request;
use http::Response;
use hyper::service::make_service_fn;
use hyper::Body;
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mspc;
use tokio::sync::oneshot;

use super::net::TcpListenerResource;

pub fn init(rt: &mut deno_core::JsRuntime) {
  super::reg_json_async(rt, "op_start_http", op_start_http);
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartHttpArgs {
  rid: u32,
}

async fn op_start_http(
  state: Rc<RefCell<OpState>>,
  args: Value,
  _zero_copy: BufVec,
) -> Result<Value, AnyError> {
  let args: StartHttpArgs = serde_json::from_value(args)?;
  let rid = args.rid as u32;

  super::check_unstable2(&state, "Deno.startHttp");

  let resource_rc = state
    .borrow_mut()
    .resource_table
    .take::<TcpListenerResource>(rid)
    .ok_or_else(bad_resource_id)?;
  let resource = Rc::try_unwrap(resource_rc)
    .expect("Only a single use of this resource should happen");
  let tcp_listener = resource.listener;

  let builder = hyper::Server::from_tcp(tcp_listener)?;
  let (sender, reciever) = mspc::channel(0);

  let svc = HttpService { sender };
  let make_svc = make_service_fn(|_| {
    let svc = svc.clone();
    async move { Ok(svc) }
  });
  let server = builder.serve(make_svc);

  let rid = {
    let mut state_ = state.borrow_mut();
    state_
      .resource_table
      .add(StreamResource::client_tls_stream(tls_stream))
  };
  Ok(json!({
      "rid": rid,
  }))
}

pub struct HttpServerResource {
  reciever: mspc::Reciever<RequestAndResponse>,
}

type RequestAndResponse = (Request<Body>, oneshot::Sender<ResponseOrError>);
type ResponseOrError = Result<Response<Body>, http::Error>;

#[derive(Clone)]
struct HttpService {
  sender: mspc::Sender<RequestAndResponse>,
}

impl hyper::service::Service<Request<Body>> for HttpService {
  type Response = Body;
  type Error = http::Error;
  type Future =
    Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

  fn poll_ready(
    &mut self,
    cx: &mut Context<'_>,
  ) -> Poll<Result<(), Self::Error>> {
    Poll::Ready(Ok(()))
  }

  fn call(&mut self, req: Request<Body>) -> Self::Future {
    let fut = async move {
      let (tx, rx) = oneshot::channel();
      self.sender.send((req, tx)).await;
      rx.await.unwrap()
    };
    fut.boxed_local()
  }
}
