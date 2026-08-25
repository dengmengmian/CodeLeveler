# Reviewer value (pilot)

Independent Reviewer vs self-verify on coding tasks with error space.

```sh
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT --mode self
leveler eval run --suite multi_agent --experiment MA-VALUE-REVIEWER-PILOT --mode reviewer
```

`--mode self` writes `agents.independent_review = "off"` into an isolated
`LEVELER_HOME`. `--mode reviewer` writes `"always"` (launch after any product
mutation). Product default stays `auto`. Reviewer remains read-only.
