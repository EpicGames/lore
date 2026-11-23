---
type: improvement
---

- `lore-transport`: a client or server that sends many requests at once over a QUIC connection,
  such as a burst of reads a server sends to its peers, sends them sooner. The network threads
  write all the requests waiting on a stream together, and a request no longer waits for the
  one before it to finish writing.
