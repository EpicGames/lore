// SPDX-FileCopyrightText: 2026 Epic Games, Inc.
// SPDX-License-Identifier: MIT
import { encodeMetadata } from "./core.js";
const utf8 = new TextEncoder();
const string = value => utf8.encode(value);
const number = value => {
  const result = new Uint8Array(8);
  new DataView(result.buffer).setBigUint64(0, BigInt(value), true);
  return result;
};

function encode(entries) {
  return encodeMetadata(entries.map(([key, type, value]) => [string(key), type, value]));
}

export function repositoryMetadata(record, branchId) {
  return encode([
    ["name", 6, string(record.name)], ["description", 6, string(record.description)],
    ["default-branch", 3, branchId], ["default-branch-name", 6, string(record.defaultBranchName)],
    ["creator", 6, string(record.creator)], ["created", 5, number(record.created)],
  ]);
}

export function branchMetadata(record, branchId) {
  return encode([
    ["id", 255, branchId], ["name", 6, string(record.defaultBranchName)],
    ["category", 6, string("")], ["creator", 6, string(record.creator)],
    ["created", 5, number(record.created)], ["protect", 2, Uint8Array.of(0)],
  ]);
}
