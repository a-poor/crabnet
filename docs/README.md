# Documentation guide

Start with the repository [README](../README.md) for the public API and examples.
Use this index to find the authoritative document for each kind of question.

## Current behavior and adoption

| Question | Document |
| --- | --- |
| What query features work? | [Coverage](coverage.md) |
| What are the exact value, query, error and transaction rules? | [Semantics](semantics.md) |
| Which reads stream, and where do they buffer? | [Cursors](cursors.md) |
| How do reusable read plans work? | [Compiled reads](compiled-reads.md) |
| Which platforms, SQLite settings and failure models are covered? | [Support](support.md) |
| How do API changes, storage upgrades and rollback work? | [Compatibility](compatibility.md) |

## Development and releases

| Question | Document |
| --- | --- |
| What should we work on now? | [Roadmap](roadmap.md) |
| What must the first release demonstrate? | [First-release checklist](first-release.md) |
| Where does code belong? | [Architecture and maintenance](architecture.md) |
| How should a change be tested? | [Testing workflow](testing.md) |
| Which suite covers a particular behavior? | [Test catalog](test-catalog.md) |
| How was the current implementation built? | [Milestone history](history/milestones.md) |

## Applications and evidence

- [Dependency-graph application](dependency-graph.md) and its lifecycle workload.
- [Git Graph pilot](git-graph-pilot.md), including its recorded acceptance scope.
- [Measurements](measurements/) and [compiled-read results](compiled-reads-results.json).
- [Historical testing notes](history/testing-notes.md) and regression provenance.
- [TCK instructions and pinned outcomes](../tests/tck/README.md).
- [Historical storage fixtures](../tests/fixtures/compatibility/README.md).

Historical reports describe the revision and configuration they measured. They
are not proof that a newer candidate has passed. Preserve raw evidence and
provenance when adding a new run. Keep current contracts in their owning document
and link to them from plans and reports rather than copying a competing version.
