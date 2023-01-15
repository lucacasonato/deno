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
      return new AtomicOperation(this.#rid);
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
      key = convertKey(key);
      // TODO(lucacasonato): implement opts
      const entries = await ops.op_state_snapshot_read_one(this.#rid, key);
      const entry = entries[0];
      deserializeValue(entry);
      return entry;
    }

    async set(key, value) {
      key = convertKey(key);
      value = serializeValue(value);

      const checks = [];
      const mutations = [
        [key, "set", value],
      ];

      const result = await ops.op_state_atomic_write(
        this.#rid,
        checks,
        mutations,
      );
      if (!result) throw new TypeError("Failed to set value");
    }

    async delete(key) {
      key = convertKey(key);

      const checks = [];
      const mutations = [
        [key, "delete", null],
      ];

      const result = await ops.op_state_atomic_write(
        this.#rid,
        checks,
        mutations,
      );
      if (!result) throw new TypeError("Failed to set value");
    }
  }

  class Queue {
    #rid;

    constructor(rid) {
      this.#rid = rid;
    }
  }

  class AtomicOperation {
    #rid;

    #checks = [];
    #mutations = [];

    constructor(rid) {
      this.#rid = rid;
    }

    check(...checks) {
      for (const check of checks) {
        this.#checks.push([convertKey(check.key), check.versionstamp]);
      }
      return this;
    }

    mutate(...mutations) {
      for (const mutation of mutations) {
        const key = convertKey(mutation.key);
        let type;
        let value;
        switch (mutation.type) {
          case "delete":
            type = "delete";
            value = null;
            break;
          case "set":
          case "sum":
          case "min":
          case "max":
            type = mutation.type;
            value = serializeValue(mutation.value);
            break;
          default:
            throw new TypeError("Invalid mutation type");
        }
        this.#mutations.push([key, type, value]);
      }
      return this;
    }

    set(key, value) {
      this.#mutations.push([convertKey(key), "set", serializeValue(value)]);
      return this;
    }

    delete(key) {
      this.#mutations.push([convertKey(key), "delete", null]);
      return this;
    }

    async commit() {
      const result = await ops.op_state_atomic_write(
        this.#rid,
        this.#checks,
        this.#mutations,
      );
      return result;
    }

    then() {
      throw new TypeError(
        "`Deno.AtomicOperation` is not a promise. Did you forget to call `commit()`?",
      );
    }
  }

  function convertKey(key) {
    if (Array.isArray(key)) {
      return key.map(convertKeyPart);
    } else {
      return convertKey([key]);
    }
  }

  function convertKeyPart(key) {
    if (typeof key === "string") {
      return key;
    } else if (typeof key === "number") {
      return key;
    } else if (typeof key === "bigint") {
      return key;
    } else if (key instanceof Uint8Array) {
      return key;
    } else {
      throw new TypeError("Invalid key type");
    }
  }

  function deserializeValue(entry) {
    if (entry.value === null) {
      entry.value = undefined;
      return;
    }
    const { kind, value } = entry.value;
    switch (kind) {
      case "v8":
        entry.value = core.deserialize(value);
        break;
      case "bool":
      case "float":
      case "int":
      case "bytes":
        entry.value = value;
        break;
      default:
        throw new TypeError("Invalid value type");
    }
  }

  function serializeValue(value) {
    if (typeof value === "boolean") {
      return {
        kind: "bool",
        value,
      };
    } else if (typeof value === "number") {
      return {
        kind: "float",
        value,
      };
    } else if (typeof value === "bigint") {
      return {
        kind: "int",
        value,
      };
    } else if (value instanceof Uint8Array) {
      return {
        kind: "bytes",
        value,
      };
    } else {
      return {
        kind: "v8",
        value: core.serialize(value),
      };
    }
  }

  window.__bootstrap.state = {
    openDatabase,
    Database,
    KvStore,
    Queue,
  };
})(globalThis);
