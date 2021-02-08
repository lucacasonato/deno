((window) => {
  const core = window.Deno.core;
  const { errors } = window.__bootstrap.errors;
  const { read, write } = window.__bootstrap.io;
  const { fastBody, dontValidateUrl } = window.__bootstrap.fetch;

  const illegalConstructorKey = Symbol("illegalConstructorKey");

  class FetchEvent extends Event {
    /** @type {Request} */
    #request;
    /** @type {number | null} */
    #requestBodyRid;
    /** @type {number} */
    #responseSenderRid;

    #responded = false;

    constructor(options) {
      if (options?.illegalConstructorKey !== illegalConstructorKey) {
        throw new TypeError("Illegal constructor.");
      }
      super("fetch", { cancelable: false, composed: false, bubbles: false });
      this.#request = options.request;
      this.#requestBodyRid = options.requestBodyRid;
      this.#responseSenderRid = options.responseSenderRid;
    }

    get request() {
      return this.#request;
    }

    set request(_) {
      // this is a no-op
    }

    /** @param {Response} response */
    respondWith(response) {
      if (this.#responded) {
        throw new TypeError("Already responded to this FetchEvent.");
      }
      this.#responded = true;
      // TODO(lucacasonato): handle the error here correctly.
      respondWith(response, this.#requestBodyRid, this.#responseSenderRid);
    }
  }

  /**
   * @param {Response} response
   * @param {number | null} requestBodyRid
   * @param {number} responseSenderRid
   */
  async function respondWith(response, requestBodyRid, responseSenderRid) {
    if (response instanceof Promise) {
      response = await response;
    }
    if (!(response instanceof Response)) {
      throw new TypeError(
        "First argument to FetchEvent#respondWith must be a Response.",
      );
    }

    const headers = [...response.headers.entries()];
    const status = response.status;

    // Get the response body. This will return either an ArrayBuffer if the data
    // is synchronously available, or it will return a ReadableStream.
    const body = response[fastBody]();

    // If the body is synchronously available, or there is no body at all, send
    // it right away.
    let response0CopyBufs;
    if (body instanceof ArrayBuffer) {
      response0CopyBufs = [new Uint8Array(body)];
    } else if (!body) {
      response0CopyBufs = [new Uint8Array(0)];
    } else {
      response0CopyBufs = [];
    }
    const { responseBodyRid } = core.jsonOpSync(
      "op_http_respond",
      { rid: responseSenderRid, headers, status },
      ...response0CopyBufs,
    );

    // If op_http_respond returns a responseBodyRid, we should stream the body
    // to that resource.
    if (typeof responseBodyRid === "number") {
      if (!body || !(body instanceof ReadableStream)) {
        throw new Error(
          "internal error: recieved responseBodyRid, but response has no body or is not a stream",
        );
      }
      for await (const chunk of body) {
        await write(responseBodyRid, chunk);
      }
    }

    if (typeof requestBodyRid === "number") {
      // After the response has been fully sent, we can close the request body.
      try {
        core.close(requestBodyRid);
      } catch (err) {
        // ignore error if the request body was already closed by the user
      }
    }

    // Once all chunks are sent, and the request body is closed, we can close
    // the response body.
    if (typeof responseBodyRid === "number") {
      core.close(responseBodyRid);
    }
  }

  async function nextFetchEvent(rid) {
    const res = await core.jsonOpAsync("op_http_next_request", { rid });
    if (res === null) return null;
    const {
      method,
      url,
      headers,
      requestBodyRid,
      responseSenderRid,
    } = res;

    /** @type {ReadableStream<Uint8Array> | undefined} */
    let body = undefined;

    if (typeof requestBodyRid === "number") {
      body = new ReadableStream({
        type: "bytes",
        async pull(controller) {
          try {
            // This is the largest possible size for a single packet on a TLS
            // stream.
            const chunk = new Uint8Array(16 * 1024 + 256);
            const result = await read(requestBodyRid, chunk);
            if (result !== null && result > 0) {
              // We read some data. Enqueue it onto the stream.
              if (chunk.length == result) {
                controller.enqueue(chunk);
              } else {
                controller.enqueue(chunk.subarray(0, result));
              }
            } else {
              // We have reached the end of the body, so we close the stream.
              controller.close();
              core.close(requestBodyRid);
            }
          } catch (err) {
            // There was an error while reading a chunk of the body, so we
            // error.
            controller.error(err);
            controller.close();
            core.close(requestBodyRid);
          }
        },
        cancel() {
          core.close(requestBodyRid);
        },
      });
    }

    const request = new Request(url, {
      method,
      headers,
      body,
      [dontValidateUrl]: true,
    });

    return new FetchEvent({
      request,
      requestBodyRid,
      responseSenderRid,
      illegalConstructorKey,
    });
  }

  function startHttp(conn) {
    const { rid } = core.jsonOpSync("op_http_start", { rid: conn.rid });
    return new HttpServer(rid);
  }

  class HttpServer {
    #rid = 0;

    constructor(rid) {
      this.#rid = rid;
    }

    get rid() {
      return this.#rid;
    }

    async next() {
      let fetchEvent;
      try {
        fetchEvent = await nextFetchEvent(this.rid);
      } catch (error) {
        if (error instanceof errors.BadResource) {
          return { value: undefined, done: true };
        }
        throw error;
      }
      if (fetchEvent === null) {
        return { value: undefined, done: true };
      }
      return { value: fetchEvent, done: false };
    }

    return(value) {
      this.close();
      return Promise.resolve({ value, done: true });
    }

    close() {
      core.close(this.rid);
    }

    [Symbol.asyncIterator]() {
      return this;
    }
  }

  window.__bootstrap.http = {
    FetchEvent,
    startHttp,
    HttpServer,
  };
})(this);
