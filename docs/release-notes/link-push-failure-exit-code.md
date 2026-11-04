---
type: breaking
---

- `lore-revision`: a failed link push exits with its error's code rather than 255, and with the new code 52 (`LinkDivergent`) when the linked branch has newer revisions on the remote. Scripts should test a failed push for any non-zero code.
