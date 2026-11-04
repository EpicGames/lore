// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
import { DurableObject } from "cloudflare:workers";
import { Code, ConnectError } from "@connectrpc/connect";
import { fromHex, putFragment, toHex } from "./immutable.js";
import { branchMetadata, repositoryMetadata } from "./metadata.js";
import { loreHash } from "./core.js";

const encoder = new TextEncoder();
const ZERO_CONTEXT = new Uint8Array(16);
const byteLength = value => encoder.encode(value).length;

function id(value, label) {
  if (value.length !== 16 || value.every(byte => byte === 0)) throw new ConnectError(`invalid ${label}`, Code.InvalidArgument);
  return toHex(value);
}
function repositoryRecord(record) {
  return { ...record, id: fromHex(record.id), defaultBranchId: fromHex(record.defaultBranchId), metadata: fromHex(record.metadata), created: BigInt(record.created) };
}
export function branchRecord(record) {
  return { ...record, id: fromHex(record.id), latest: fromHex(record.latest), metadata: fromHex(record.metadata), created: BigInt(record.created), stack: [] };
}

// Names are a global coordination atom. Entries remain reserved through a crash
// until the same request retries, and become visible only after initialization.
export class RepositoryCatalog extends DurableObject {
  constructor(ctx, env) {
    super(ctx, env);
    this.ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS repositories (id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, request TEXT NOT NULL, created INTEGER NOT NULL, metadata TEXT)");
  }
  reserve(request) {
    return this.ctx.storage.transactionSync(() => {
      const sql = this.ctx.storage.sql;
      const prior = sql.exec("SELECT * FROM repositories WHERE id = ? OR name = ?", request.id, request.name).toArray();
      if (prior.length) {
        if (prior.length !== 1 || prior[0].request !== JSON.stringify(request)) return { conflict: true };
        return { created: prior[0].created, metadata: prior[0].metadata };
      }
      const created = Date.now();
      sql.exec("INSERT INTO repositories (id, name, request, created) VALUES (?, ?, ?, ?)", request.id, request.name, JSON.stringify(request), created);
      return { created, metadata: null };
    });
  }
  finish(id, metadata) {
    this.ctx.storage.sql.exec("UPDATE repositories SET metadata = COALESCE(metadata, ?) WHERE id = ?", metadata, id);
  }
  get(kind, value) {
    const row = this.ctx.storage.sql.exec(`SELECT * FROM repositories WHERE ${kind === "id" ? "id" : "name"} = ? AND metadata IS NOT NULL`, value).toArray()[0];
    return row ? { ...JSON.parse(row.request), created: row.created, metadata: row.metadata } : null;
  }
  list(creator) {
    return this.ctx.storage.sql.exec("SELECT * FROM repositories WHERE metadata IS NOT NULL ORDER BY name").toArray()
      .map(row => ({ ...JSON.parse(row.request), created: row.created, metadata: row.metadata }))
      .filter(record => creator === null || record.creator === creator);
  }
}

function catalog(env) { return env.CATALOG.getByName("repositories"); }
async function writeMetadata(env, scope, payload) {
  const address = { hash: loreHash(payload), context: ZERO_CONTEXT };
  await putFragment(env, scope, address, { flags: 0, sizePayload: payload.length, sizeContent: BigInt(payload.length) }, payload);
  return toHex(address.hash);
}

export function repositoryService(env) {
  return {
    async repositoryCreate(req) {
      const repoId = id(req.id, "repository id");
      const branchId = id(req.defaultBranchId, "default branch id");
      if (byteLength(req.name) > 1000 || !/^[A-Za-z0-9_.\/-]+$/.test(req.name) || req.name.split("/").some(part => !part || part.startsWith(".")) ||
          byteLength(req.description) > 65536 || !req.defaultBranchName || byteLength(req.defaultBranchName) > 1000 || byteLength(req.creator ?? "") > 1000) {
        throw new ConnectError("invalid repository creation fields", Code.InvalidArgument);
      }
      const request = { id: repoId, name: req.name, description: req.description, defaultBranchId: branchId, defaultBranchName: req.defaultBranchName, creator: req.creator ?? "" };
      const reservation = await catalog(env).reserve(request);
      if (reservation.conflict) throw new ConnectError("repository id or name already exists", Code.AlreadyExists);
      const record = { ...request, created: reservation.created };
      const scope = { id: repoId, store: env.MUTABLE.getByName(repoId) };
      const metadata = await writeMetadata(env, scope, repositoryMetadata(record, req.defaultBranchId));
      const branchHash = await writeMetadata(env, scope, branchMetadata(record, req.defaultBranchId));
      await scope.store.initializeRepository(request, reservation.created, metadata, branchHash);
      await catalog(env).finish(repoId, metadata);
      return { repository: repositoryRecord({ ...record, metadata }) };
    },
    async repositoryGet(req) {
      const query = req.query;
      if (!query || !query.case) throw new ConnectError("repository query required", Code.InvalidArgument);
      const value = query.case === "id" ? id(query.value, "repository id") : query.value;
      const record = await catalog(env).get(query.case, value);
      if (!record) throw new ConnectError("repository not found", Code.NotFound);
      return { repository: repositoryRecord(record) };
    },
    async *repositoryList(req) {
      for (const record of await catalog(env).list(req.creator ?? null)) yield { repository: repositoryRecord(record) };
    },
    async repositoryMetadataGet(req) {
      const record = await catalog(env).get("id", id(req.id, "repository id"));
      if (!record) throw new ConnectError("repository not found", Code.NotFound);
      return { metadata: fromHex(record.metadata) };
    },
  };
}
