---
type: feature
---

- `lore-server`: The repository content and presigned redeem endpoints serve the single byte range
  a `GET` request's `Range` header asks for with `206 Partial Content`, and answer a range that
  selects no byte of non-empty content with `416 Range Not Satisfiable`. A request with several
  ranges, or with `If-Range`, gets the whole content. `200`, `206` and `416` responses carry
  `Accept-Ranges: bytes`. A `HEAD` request to the repository content endpoint returns its headers
  without streaming the content.
