---
type: feature
---

- `lore-server`: `[server.quic]` and `[server.quic_internal]` take a `congestion_algorithm` key,
  `bbr` or `cubic`, which chooses the congestion controller for what the endpoint sends, such as
  the data of a get. Unset, the public endpoint keeps BBR and the internal endpoint Cubic, as
  before. Cubic suits a path that drops datagrams under load, such as Docker's userland proxy,
  where BBR sends far more bytes than the data holds.
- `lore-server`: A `remote` store's settings (`[immutable_store.remote]`, `[mutable_store.remote]`)
  take `congestion_algorithm` too: the controller a connection to the upstream runs when the store
  establishes it, for what the server sends there. Unset, BBR, as before. So does
  `[immutable_store.replicated]`, for what the server replicates to its peer; unset, Cubic, as
  before. A connection already established is reused as it is.
- `lore-transport`: The `LORE_QUIC_CONGESTION_ALGORITHM` environment variable, `bbr` or `cubic`,
  chooses the controller a QUIC storage connection runs when a client with no settings to name
  one establishes it, as the CLI and the storage API do. It paces what the client sends, such as
  the data of a put. Unset or blank, it is BBR, as before. With any other value, the connections
  such a client establishes fail, naming the variable.
