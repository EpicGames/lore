---
type: breaking
---

- `lore-transport`: the user agent is `[application] [product] lore/<library version>`, so the library version is
  always present. `LORE_USER_AGENT` names the application and goes in front of the Lore token rather than replacing
  the whole string. `LORE_USER_AGENT_PRODUCT` names the product, with a fallback supplied by the binary. If neither is
  set, the product is omitted rather than defaulting to `lore-transport`. A client that sets neither sends
  `lore/<version>` instead of `lore-transport/<version>`. A server with `user_agent_patterns` configured records a
  client matching none of them as `<unknown>` in metrics, so it needs patterns for the new form before clients built
  from this release connect. `user_agent_product()` returns an `Option`, and
  `make_user_agent()` and `make_user_agent_with_component()` are replaced by `user_agent_with_component()`
