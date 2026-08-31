---
status: superseded by [ADR-00018](00018-fragment-metadata-on-s3-objects.md)
date: 2024-05-19
deciders: Mattias Jansson
---

# ADR-00006: Reconsider S3 storage model

## Context and Problem Statement

In [ADR-00004](00004-s3-storage-options.md), we decided on the following storage model for immutable
fragments:

- Fragment payloads would be stored with `<hash>` as the S3 Object key.
- Fragment metadata would be stored in with an S3 Object key of `<hash>/<repository id>/<context>`.

This would result in metadata being duplicated for each repository/context combo that referenced the
same fragment data. In an internal pull-request discussion, the question was
raised of whether we wanted to replicate the fragment metadata in this way.

## Decision Drivers

- Minimize the implementation complexity for the S3 store.
- Minimize the associated AWS costs resulting from the S3 store layout and query patterns.
- Ensure we can reasonably rewrite fragment metadata when necessary (for example, if a fragment is
  compressed after being added to the CAS, resulting in a change to its flags)
- Ensure correctness if the same fragment is used by clients making different compression decisions.

## Considered Options

- Stick with the agreed upon plan for duplicating fragment metadata for each repository/context
  pair.
- Store the fragment metadata once, separate from the fragment payload.
- Store the fragment metadata once, together with the fragment payload.

## Decision Outcome

Chosen option: "Store the fragment metadata once, together with the fragment payload," because it
ensures correctness across clients while minimizing implementation complexity and cost.

### Consequences

- Good, because the fragment metadata is stored once, meaning there are no concerns about it getting
  out of sync with the stored payload.
- Good, because the fragment metadata is stored once, we only need to look in one place if it needs
  to be updated.
- Neutral, because we must store an empty sentinel object to represent the association between a
  fragment and a repository/context pair.

## Pros and Cons of the Options

### Continue duplicating fragment metadata for each repository/context pair

- Good, because it's simple to implement and offers good cost efficiency.
- Bad, because it makes updating metadata for all repository/context pairs that reference a fragment
  complex.
- Bad, because it leaves open the possibility of the metadata for one repository/context pair being
  out of sync with the data actually stored in the fragment payload, risking client
  errors if the metadata flags don't match what's stored in the payload object.

### Store the fragment metadata once, separate from the fragment payload

In this scenario we create a separate S3 Object for the metadata keyed as `<hash>/metadata`.

- Good, because the fragment metadata is stored once, meaning there are no concerns about it getting
  out of sync with the stored payload.
- Good, because the fragment metadata is stored once, we only need to look in one place if it needs
  to be updated.
- Bad, because it has the highest amount of complexity and S3 cost (it's worth noting that the S3
  cost for all of these options is likely trivial in the overall scheme of things, so this is judged
  relative to the other options).

### Store the fragment metadata once, together with the fragment payload

In this scenario we store the metadata in the same S3 Object as the fragment payload, as a 16 byte
preamble.

- Good, because the fragment metadata is stored once, meaning there are no concerns about it getting
  out of sync with the stored payload.
- Good, because the fragment metadata is stored once, we only need to look in one place if it needs
  to be updated.
- Neutral, because we must store an empty sentinel object to represent the association between a
  fragment and a repository/context pair.
