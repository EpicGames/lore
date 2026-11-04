// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
// Unauthenticated experimental subset of the Lore service APIs.
import { DurableObject } from "cloudflare:workers";
import { Code, ConnectError, createConnectRouter } from "@connectrpc/connect";
import { universalServerRequestFromFetch, universalServerResponseToFetch } from "@connectrpc/connect/protocol";
import { EnvironmentService } from "../../src/gen/lore/environment/v1/environment_pb.js";
import { StorageService } from "../../src/gen/lore/storage/v1/storage_pb.js";
import { RepositoryService } from "../../src/gen/lore/repository/v1/repository_pb.js";
import { RevisionService } from "../../src/gen/lore/revision/v1/revision_pb.js";
import { fromHex, getFragment, itemStatus, putFragment, queryFragments, toHex } from "./immutable.js";
import { branchRecord, repositoryService } from "./repository.js";
import { verifyRevision } from "./revision.js";
export { RepositoryCatalog } from "./repository.js";

const ZERO = "00".repeat(32);
const hex = bytes => Array.from(bytes, b => b.toString(16).padStart(2, "0")).join("");
const bytes = value => Uint8Array.from(value.match(/../g), part => parseInt(part, 16));

function hash(value, label) {
  if (value.length !== 32) throw new ConnectError(`${label} must be 32 bytes`, Code.InvalidArgument);
  return hex(value);
}

function keyType(value, write = false) {
  if (value > 7) throw new ConnectError("invalid key type", Code.InvalidArgument);
  // These types have dedicated, validated mutation APIs in the Lore server.
  if (write && (value === 1 || value === 3 || value === 4)) {
    throw new ConnectError("key type requires its dedicated API", Code.InvalidArgument);
  }
  return value;
}

function partition(headers, env) {
  const encoded = headers.get("lore-partition-bin") ?? headers.get("urc-repository-id-bin");
  if (!encoded) throw new ConnectError("missing repository ID", Code.InvalidArgument);
  let id;
  try {
    id = Uint8Array.from(atob(encoded), c => c.charCodeAt(0));
  } catch {
    throw new ConnectError("invalid repository ID", Code.InvalidArgument);
  }
  if (id.length !== 16) throw new ConnectError("repository ID must be 16 bytes", Code.InvalidArgument);
  const name = hex(id);
  return { id: name, store: env.MUTABLE.getByName(name) };
}

// One instance per partition. Synchronous SQLite transactions make each CAS atomic,
// including the read of a missing key and a concurrent insert.
export class MutablePartition extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS entries (type INTEGER NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (type, key))");
    this.ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS fragments (hash TEXT NOT NULL, context TEXT NOT NULL, PRIMARY KEY (hash, context))");
    this.ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS branches (id TEXT PRIMARY KEY, name TEXT COLLATE NOCASE UNIQUE NOT NULL, record TEXT NOT NULL)");
  }

  load(type, key) {
    return this.ctx.storage.sql.exec("SELECT value FROM entries WHERE type = ? AND key = ?", type, key).toArray()[0]?.value ?? null;
  }

  store(type, key, value) {
    if (value === ZERO) {
      this.ctx.storage.sql.exec("DELETE FROM entries WHERE type = ? AND key = ?", type, key);
    } else {
      this.ctx.storage.sql.exec("INSERT INTO entries (type, key, value) VALUES (?, ?, ?) ON CONFLICT(type, key) DO UPDATE SET value = excluded.value", type, key, value);
    }
  }

  compareAndSwap(type, key, expected, value) {
    return this.ctx.storage.transactionSync(() => {
      const current = this.load(type, key) ?? ZERO;
      // Unlike store(zero), CAS can install an explicit zero value. Lore uses
      // this to initialize branch pointers and subsequently reads that value.
      if (current === expected) {
        this.ctx.storage.sql.exec("INSERT INTO entries (type, key, value) VALUES (?, ?, ?) ON CONFLICT(type, key) DO UPDATE SET value = excluded.value", type, key, value);
      }
      return current;
    });
  }

  associate(hash, context) {
    this.ctx.storage.sql.exec("INSERT OR IGNORE INTO fragments (hash, context) VALUES (?, ?)", hash, context);
  }

  matchFragment(hash, context) {
    const matches = this.ctx.storage.sql.exec("SELECT context FROM fragments WHERE hash = ? ORDER BY context = ? DESC LIMIT 1", hash, context).toArray();
    return matches.length === 0 ? 2 : matches[0].context === context ? 0 : 1;
  }

  initializeRepository(request, created, metadata, branchHash) {
    this.ctx.storage.transactionSync(() => {
      const existing = this.ctx.storage.sql.exec("SELECT record FROM branches WHERE id = ?", request.defaultBranchId).toArray()[0];
      const record = {
        id: request.defaultBranchId, name: request.defaultBranchName, creator: request.creator,
        category: "", created, latest: ZERO, deleted: false, metadata: branchHash,
      };
      if (existing) {
        const prior = JSON.parse(existing.record);
        if (prior.id !== record.id || prior.name !== record.name || prior.creator !== record.creator || prior.created !== record.created || prior.metadata !== record.metadata) {
          throw new Error("repository initialization conflict");
        }
      }
      if (!existing) this.ctx.storage.sql.exec("INSERT INTO branches (id, name, record) VALUES (?, ?, ?)", record.id, record.name, JSON.stringify(record));
    });
  }

  getBranch(kind, value) {
    const row = this.ctx.storage.sql.exec(`SELECT record FROM branches WHERE ${kind === "id" ? "id" : "name"} = ?`, value).toArray()[0];
    return row ? JSON.parse(row.record) : null;
  }

  listBranches(creator) {
    return this.ctx.storage.sql.exec("SELECT record FROM branches ORDER BY name").toArray()
      .map(row => JSON.parse(row.record)).filter(record => creator === null || record.creator === creator);
  }

  advanceBranch(id, expected, next, number) {
    return this.ctx.storage.transactionSync(() => {
      const row = this.ctx.storage.sql.exec("SELECT record FROM branches WHERE id = ?", id).toArray()[0];
      if (!row) return { result: "missing" };
      const record = JSON.parse(row.record);
      if (record.latest === next) return { result: "ok" };
      if (record.latest !== expected || BigInt(record.number ?? "0") + 1n !== BigInt(number)) return { result: "diverged", latest: record.latest };
      record.latest = next;
      record.number = number;
      this.ctx.storage.sql.exec("UPDATE branches SET record = ? WHERE id = ?", JSON.stringify(record), id);
      return { result: "ok" };
    });
  }
}

export default {
  async fetch(request, env) {
    const router = createConnectRouter();
    router.service(EnvironmentService, { environmentGet: () => ({ environment: { config: { compressionMode: 1, maxQueryBatch: 5461 } } }) });
    router.service(RepositoryService, repositoryService(env));
    router.service(RevisionService, {
      async branchGet(req, context) {
        if (!req.query?.case) throw new ConnectError("branch query required", Code.InvalidArgument);
        let value = req.query.value;
        if (req.query.case === "id") {
          if (value.length !== 16) throw new ConnectError("invalid branch id", Code.InvalidArgument);
          value = hex(value);
        }
        const record = await partition(context.requestHeader, env).store.getBranch(req.query.case, value);
        if (!record) throw new ConnectError("branch not found", Code.NotFound);
        return { branch: branchRecord(record) };
      },
      async *branchList(req, context) {
        const records = await partition(context.requestHeader, env).store.listBranches(req.creator ?? null);
        for (const record of records) if (req.includeDeleted || !record.deleted) yield { branch: branchRecord(record) };
      },
      async branchMetadataGet(req, context) {
        if (req.id.length !== 16) throw new ConnectError("invalid branch id", Code.InvalidArgument);
        const record = await partition(context.requestHeader, env).store.getBranch("id", hex(req.id));
        if (!record) throw new ConnectError("branch not found", Code.NotFound);
        return { metadata: bytes(record.metadata) };
      },
      async branchPush(req, context) {
        if (req.id.length !== 16 || req.revisionSignature.length !== 32 || req.revisionSignature.every(byte => byte === 0)) {
          throw new ConnectError("invalid branch or revision", Code.InvalidArgument);
        }
        if (req.force || req.fastForwardMerge) throw new ConnectError("forced pushes and merges are not supported yet", Code.Unimplemented);
        const scope = partition(context.requestHeader, env);
        const branchId = hex(req.id);
        const record = await scope.store.getBranch("id", branchId);
        if (!record) throw new ConnectError("branch not found", Code.NotFound);
        const verified = await verifyRevision(env, scope, req.revisionSignature);
        const result = await scope.store.advanceBranch(branchId, verified.parent, hex(req.revisionSignature), String(verified.number));
        if (result.result === "missing") throw new ConnectError("branch not found", Code.NotFound);
        if (result.result !== "ok") throw new ConnectError(`branch push is not a fast-forward; branch latest: ${result.latest}`, Code.FailedPrecondition);
        return { revisionSignature: req.revisionSignature, revisionNumber: verified.number, fastForwardMerged: false };
      },
    });
    router.service(StorageService, {
      async *put(requests, context) {
        const scope = partition(context.requestHeader, env);
        for await (const req of requests) {
          if (!req.address) throw new ConnectError("put address is required", Code.InvalidArgument);
          try {
            await putFragment(env, scope, req.address, req.fragment, req.payload);
            yield { address: req.address, status: { code: 0 } };
          } catch (error) {
            yield { address: req.address, status: itemStatus(error) };
          }
        }
      },
      async *get(requests, context) {
        const scope = partition(context.requestHeader, env);
        for await (const address of requests) {
          try {
            yield { address, ...await getFragment(env, scope, address), status: { code: 0 } };
          } catch (error) {
            yield { address, status: itemStatus(error) };
          }
        }
      },
      async *getMetadata(requests, context) {
        const scope = partition(context.requestHeader, env);
        for await (const address of requests) {
          try {
            yield { address, ...await getFragment(env, scope, address, true), status: { code: 0 } };
          } catch (error) {
            yield { address, status: itemStatus(error) };
          }
        }
      },
      async query(req, context) {
        return queryFragments(partition(context.requestHeader, env), req.addresses);
      },
      async *putResolved(requests, context) {
        const scope = partition(context.requestHeader, env);
        for await (const req of requests) {
          if (req.requestId === 0n) throw new ConnectError("request id must be nonzero", Code.InvalidArgument);
          try {
            const key = hash(req.key, "key");
            if (key === ZERO || !req.address) throw new ConnectError("invalid key or missing address", Code.InvalidArgument);
            const target = hash(req.address.hash, "address hash");
            if (target !== ZERO) await putFragment(env, scope, req.address, req.fragment, req.payload);
            await scope.store.store(7, key, target);
            yield { requestId: req.requestId, status: { code: 0 } };
          } catch (error) {
            yield { requestId: req.requestId, status: itemStatus(error) };
          }
        }
      },
      async *getResolved(requests, context) {
        const scope = partition(context.requestHeader, env);
        for await (const req of requests) {
          if (req.requestId === 0n) throw new ConnectError("request id must be nonzero", Code.InvalidArgument);
          try {
            if (req.flags !== 0) throw new ConnectError("unsupported flags", Code.Unimplemented);
            if (!req.key) throw new ConnectError("key is required", Code.InvalidArgument);
            const resolved = await scope.store.load(7, hash(req.key.hash, "key"));
            if (resolved === null) throw new ConnectError("mutable key not found", Code.NotFound);
            const data = await getFragment(env, scope, { hash: fromHex(resolved), context: req.key.context });
            yield { requestId: req.requestId, resolved: fromHex(resolved), ...data, status: { code: 0 } };
          } catch (error) {
            yield { requestId: req.requestId, status: itemStatus(error) };
          }
        }
      },
      async *copy(requests, context) {
        const scope = partition(context.requestHeader, env);
        for await (const req of requests) {
          if (!req.sourceAddress) throw new ConnectError("copy source address is required", Code.InvalidArgument);
          try {
            if (req.sourceRepositoryId.length !== 16 || toHex(req.sourceRepositoryId) !== scope.id) {
              throw new ConnectError("cross-repository copy is not supported yet", Code.Unimplemented);
            }
            await getFragment(env, scope, req.sourceAddress, true);
            const targetContext = req.targetContext.length ? req.targetContext : req.sourceAddress.context;
            if (targetContext.length !== 16) throw new ConnectError("target context must be 16 bytes", Code.InvalidArgument);
            await scope.store.associate(hash(req.sourceAddress.hash, "hash"), hex(targetContext));
            yield { sourceRepositoryId: req.sourceRepositoryId, sourceAddress: req.sourceAddress, status: { code: 0 } };
          } catch (error) {
            yield { sourceRepositoryId: req.sourceRepositoryId, sourceAddress: req.sourceAddress, status: itemStatus(error) };
          }
        }
      },
      async mutableLoad(req, context) {
        const value = await partition(context.requestHeader, env).store.load(keyType(req.keyType), hash(req.key, "key"));
        if (value === null) throw new ConnectError("mutable key not found", Code.NotFound);
        return { value: bytes(value) };
      },
      async mutableStore(req, context) {
        await partition(context.requestHeader, env).store.store(keyType(req.keyType, true), hash(req.key, "key"), hash(req.value, "value"));
        return {};
      },
      async mutableCompareAndSwap(req, context) {
        const current = await partition(context.requestHeader, env).store.compareAndSwap(
          keyType(req.keyType, true), hash(req.key, "key"), hash(req.expected, "expected"), hash(req.value, "value"),
        );
        return { currentValue: bytes(current) };
      },
    });
    const handler = router.handlers.find(item => item.requestPath === new URL(request.url).pathname);
    if (!handler) return new Response("Not found", { status: 404 });
    return universalServerResponseToFetch(await handler(universalServerRequestFromFetch(request, {})));
  },
};
