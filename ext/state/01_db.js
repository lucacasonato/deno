// Copyright 2018-2022 the Deno authors. All rights reserved. MIT license.

"use strict";

((window) => {
  const core = Deno.core;
  const ops = core.ops;

  async function openDatabase(path) {
    const rid = await ops.op_state_database_open(path);
    return new Database(rid);
  }

  class Database {
    #rid;

    #kv;
    #queue;

    constructor(rid) {
      this.#rid = rid;
      this.#kv = new KvStore(rid);
      this.#queue = new Queue(rid);
    }

    atomic() {
      throw new TypeError("TODO");
    }

    get kv() {
      return this.#kv;
    }

    get queue() {
      return this.#queue;
    }

    close() {
      core.close(this.#rid);
    }
  }

  class KvStore {
    #rid;

    constructor(rid) {
      this.#rid = rid;
    }

    async get(key, opts) {
      key = serializeKey(key);
    }
  }

  function serializeKey(key) {
    if (Array.isArray(key)) {
      return key.map(serializeKeyPart);
    } else {
      return serializeKey([key]);
    }
  }

  function serializeKeyPart(key) {
    if (typeof key === "string") {
      return new TextEncoder().encode(key);
    } else {
      throw new TypeError("Invalid key type");
    }
  }

  window.__bootstrap.state = {
    openDatabase,
    Database,
  };
})(globalThis);
