# openCypher TCK run

`tests/tck.rs` executes every scenario of the openCypher Technology
Compatibility Kit against an in-memory database and pins the outcome.

## Provenance

`vendor/openCypher-2024.3/` is an unmodified copy of the `tck/` directory from
the openCypher `2024.3` release, together with its `LICENSE` and `NOTICE`
(Apache License 2.0). It was taken from the `spec/vendor` tree shipped with the
`open-cypher` parser crate so the parser and the conformance suite describe the
same grammar revision. Re-vendor deliberately, in its own commit, when moving to
a newer TCK release.

## Files

- `allowlist.txt`: scenarios that must pass. Each line is a scenario ID of the
  form `relative/feature/path.feature::<scenario name>`, with `::example N`
  appended for scenario-outline rows.
- `exclusions.txt`: every other scenario, tab-separated from the observed
  reason it does not pass. Reasons are stable, generated classifications such
  as `unsupported: function \`date\``, `result-mismatch: row values differ`,
  or `expected-error-not-raised`, so the file doubles as a conformance backlog.
- `gherkin.rs`: a small Gherkin reader for the subset the TCK uses.
- `notation.rs`: the TCK's expected-value notation and structural comparison
  against public `CypherValue`s.

## Contract

The test fails when the observed behavior disagrees with the pinned files in
either direction: a regression, a newly passing scenario, a changed exclusion
reason, or a scenario missing from both files. This keeps exclusions explicit
and forces every promotion to be reviewed.

```sh
cargo test --test tck                      # verify the pins
CRABNET_TCK_UPDATE=1 cargo test --test tck # regenerate both files, then review the diff
CRABNET_TCK_TRACE=1 cargo test --test tck -- --nocapture  # print each scenario ID as it starts
```

Crabnet's finite-number contract explicitly rejects division/remainder by zero
and non-finite calculation results with `Error::Argument`. It does not implement
Cypher NaN/infinity values. The eight NaN comparison scenarios and four mixed-type
ordering scenarios containing `0.0 / 0.0` remain excluded as
`argument-error: division by zero`. Seven were previously wrong-answer cases;
the cross-kind NaN ordering example previously passed only because division
returned null. The four mixed-type ordering cases were already excluded for a
different error. These changes record deliberate rejection, not new conformance.

Strict collection-input checks promote seven `List6` scenarios that require
`size()` on a pattern predicate to fail at compile time. Pattern predicates
remain boolean expressions; they cannot supply the list or string that `size`
requires. The eighth case still uses unsupported variable-length pattern
predicates and remains excluded. No existing passing scenario was demoted.

## Step semantics

- `Given an empty graph` and `Given any graph` both start empty. Named graphs
  execute the vendored `graphs/<name>/<name>.cypher` file.
- `the result should be, in any order` compares rows as a bag; `in order`
  compares them positionally; `ignoring element order for lists` compares
  lists as bags at every depth.
- Nodes, relationships, and paths compare structurally by labels, type,
  properties, and path direction, never by element ID.
- Side effects are computed by diffing full graph snapshots taken around the
  query. Properties count per entity, with a changed value counting as one
  added and one removed; labels count by distinct name, as the TCK expects.
- An expected `SyntaxError` at compile time accepts `Error::Parse`,
  `Error::Semantic`, or `Error::MissingParameter`. Runtime errors also accept
  `Error::Data`, `Error::Argument`, `Error::Type`, and `Error::Constraint`. `Error::Unsupported` never satisfies an expected error, so a
  construct the engine rejects for lack of support stays excluded rather than
  counting as conformance.
