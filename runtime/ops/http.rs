use std::borrow::Cow;
use std::cell::RefCell;
use std::convert::Infallible;
use std::rc::Rc;

use deno_core::error::bad_resource_id;
use deno_core::error::generic_error;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::AsyncRefCell;
use deno_core::BufVec;
use deno_core::JsRuntime;
use deno_core::OpState;
use deno_core::RcRef;
use deno_core::Resource;
use deno_core::ZeroCopyBuf;

use deno_core::futures::StreamExt;
use hyper::body::HttpBody;
use hyper::server::conn::Http;
use hyper::service::service_fn;
use hyper::Body;
use hyper::Request;
use hyper::Response;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::io::StreamReader;

use super::io::BytesStream;
use super::io::HttpRequestBodyResource;
use super::io::HttpResponseBodyResource;

pub type RequestAndResponse = (Request<Body>, oneshot::Sender<Response<Body>>);

pub fn op_http_start(
  state: &mut OpState,
  args: Value,
  _data: &mut [ZeroCopyBuf],
) -> Result<Value, AnyError> {
  #[derive(Deserialize)]
  #[serde(rename_all = "camelCase")]
  struct Args {
    rid: u32,
  }
  let args: Args = serde_json::from_value(args)?;
  let rid = args.rid;

  let resource = state
    .resource_table
    .take::<super::io::TcpStreamResource>(rid)
    .ok_or_else(bad_resource_id)?;

  let tcp_stream = Rc::try_unwrap(resource)
    .ok()
    .ok_or_else(|| generic_error("Stream is already in use"))?;

  let (tcp_stream_read, tcp_stream_write) = tcp_stream.into_inner();
  let tcp_stream = tcp_stream_read.reunite(tcp_stream_write)?;

  let (tx, rx) = mpsc::channel::<RequestAndResponse>(1);

  let svc = service_fn(move |req: Request<Body>| {
    let tx = tx.clone();
    async move {
      let (resp_tx, resp_rx) = oneshot::channel();
      tx.send((req, resp_tx)).await.unwrap();
      let resp = resp_rx.await.unwrap();
      Ok::<Response<Body>, Infallible>(resp)
    }
  });

  tokio::task::spawn(async move {
    if let Err(http_err) = Http::new().serve_connection(tcp_stream, svc).await {
      eprintln!("Error while serving HTTP connection: {}", http_err);
    }
  });

  let rid = state.resource_table.add(HttpListener {
    listener: AsyncRefCell::new(rx),
  });

  Ok(json!({ "rid": rid }))
}

#[allow(clippy::await_holding_refcell_ref)]
pub async fn op_http_next_request(
  state: Rc<RefCell<OpState>>,
  args: Value,
  _data: BufVec,
) -> Result<Value, AnyError> {
  #[derive(Deserialize)]
  #[serde(rename_all = "camelCase")]
  struct Args {
    rid: u32,
  }
  let args: Args = serde_json::from_value(args)?;
  let rid = args.rid;

  let (req, tx) = {
    let resource = state
      .borrow()
      .resource_table
      .get::<HttpListener>(rid)
      .ok_or_else(bad_resource_id)?;

    let mut listener =
      RcRef::map(&resource, |r| &r.listener).borrow_mut().await;

    match listener.recv().await {
      None => return Ok(json!(null)),
      Some(v) => v,
    }
  };

  let method = req.method().to_string();

  let mut headers = Vec::with_capacity(req.headers().len());

  for (name, value) in req.headers().iter() {
    let name = name.to_string();
    let value = value.to_str().unwrap_or("").to_string();
    headers.push((name, value));
  }

  let host = extract_host(&req).expect("HTTP request without Host header");
  let path = req.uri().path_and_query().unwrap();
  let url = format!("https://{}{}", host, path);

  let has_body = if let Some(exact_size) = req.size_hint().exact() {
    exact_size > 0
  } else {
    true
  };

  let maybe_request_body_rid = if has_body {
    let stream: BytesStream = Box::pin(req.into_body().map(|r| {
      r.map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))
    }));
    let stream_reader = StreamReader::new(stream);
    let mut state = state.borrow_mut();
    let request_body_rid = state
      .resource_table
      .add(HttpRequestBodyResource::from(stream_reader));
    Some(request_body_rid)
  } else {
    None
  };

  let mut state = state.borrow_mut();
  let response_sender_rid =
    state.resource_table.add(HttpResponseSenderResource(tx));

  let req_json = json!({
    "requestBodyRid": maybe_request_body_rid,
    "responseSenderRid": response_sender_rid,
    "method": method,
    "headers": headers,
    "url": url,
  });

  Ok(req_json)
}

pub fn op_http_respond(
  state: &mut OpState,
  value: Value,
  data: &mut [ZeroCopyBuf],
) -> Result<Value, AnyError> {
  #[derive(Deserialize)]
  struct Args {
    rid: u32,
    status: u16,
    headers: Vec<(String, String)>,
  }

  let Args {
    rid,
    status,
    headers,
  } = serde_json::from_value(value)?;

  let response_sender = state
    .resource_table
    .take::<HttpResponseSenderResource>(rid)
    .ok_or_else(bad_resource_id)?;
  let response_sender = Rc::try_unwrap(response_sender)
    .ok()
    .expect("multiple op_respond ongoing");

  let mut builder = Response::builder().status(status);

  for (name, value) in headers {
    builder = builder.header(&name, &value);
  }

  let res;
  let maybe_response_body_rid = match data.len() {
    0 => {
      // If no body is passed, we return a writer for streaming the body.
      let (sender, body) = Body::channel();
      res = builder.body(body)?;

      let response_body_rid = state
        .resource_table
        .add(HttpResponseBodyResource::from(sender));

      Some(response_body_rid)
    }
    1 => {
      // If a body is passed, we use it, and don't return a body for streaming.
      res = builder.body(Vec::from(&*data[0]).into())?;
      None
    }
    _ => panic!("Invalid number of arguments"),
  };

  // oneshot::Sender::send(v) returns |v| on error, not an error object.
  // The only failure mode is the receiver already having dropped its end
  // of the channel.
  if response_sender.0.send(res).is_err() {
    eprintln!("op_respond: receiver dropped");
    return Err(type_error("internal communication error"));
  }

  Ok(serde_json::json!({
    "responseBodyRid": maybe_response_body_rid
  }))
}

struct HttpListener {
  listener: AsyncRefCell<mpsc::Receiver<RequestAndResponse>>,
}

impl Resource for HttpListener {
  fn name(&self) -> Cow<str> {
    "httpListener".into()
  }
}

struct HttpResponseSenderResource(oneshot::Sender<Response<Body>>);

impl Resource for HttpResponseSenderResource {
  fn name(&self) -> Cow<str> {
    "httpResponseSender".into()
  }
}

fn extract_host(req: &Request<Body>) -> Option<String> {
  if req.version() == hyper::Version::HTTP_2 {
    req.uri().host().map(|s| {
      format!(
        "{}{}",
        s,
        if let Some(port) = req.uri().port_u16() {
          format!(":{}", port)
        } else {
          "".to_string()
        }
      )
    })
  } else {
    let host_header = req.headers().get(hyper::header::HOST)?;
    Some(host_header.to_str().ok()?.to_string())
  }
}

pub fn init(rt: &mut JsRuntime) {
  super::reg_json_sync(rt, "op_http_start", op_http_start);
  super::reg_json_async(rt, "op_http_next_request", op_http_next_request);
  super::reg_json_sync(rt, "op_http_respond", op_http_respond);
}
