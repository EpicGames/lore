---
type: feature
---

- `helm`: The chart deploys edge servers with `backend.type: edge`: caches in front of another Lore deployment, with
  no storage of their own. Each edge caches content on its `/data` volume, fetches misses from the upstream's
  replication endpoint, and gets branch and repository state from the upstream, logged in as an identity seeded from
  `backend.edge.identity.secretName`. Edges can peer with each other like the AWS backend's servers.
