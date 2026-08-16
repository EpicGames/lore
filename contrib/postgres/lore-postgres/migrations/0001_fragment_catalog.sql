-- SPDX-FileCopyrightText: 2026 Epic Games, Inc.
-- SPDX-License-Identifier: MIT
-- Canonical schema for checksum/review. The implementation substitutes a validated schema name.

CREATE TABLE fragment_metadata (
    hash BYTEA PRIMARY KEY CHECK (octet_length(hash) = 32),
    flags BIGINT NOT NULL CHECK (flags BETWEEN 0 AND 4294967295),
    size_payload BIGINT NOT NULL CHECK (size_payload BETWEEN 0 AND 4294967295),
    size_content NUMERIC(20, 0) NOT NULL
        CHECK (size_content BETWEEN 0 AND 18446744073709551615)
);

CREATE TABLE fragment_association (
    hash BYTEA NOT NULL REFERENCES fragment_metadata(hash) ON DELETE RESTRICT,
    repository BYTEA NOT NULL CHECK (octet_length(repository) = 16),
    context BYTEA NOT NULL CHECK (octet_length(context) = 16),
    PRIMARY KEY (hash, repository, context)
);
