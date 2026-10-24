# VLite 0.4.1 upgrade and rollback

This is the first published release of the 0.4 concurrency and stability series.
It includes the [0.4.0 feature set and upgrade guidance](RELEASE-0.4.0.md),
plus a coordinator admission-ordering correction found during tag CI.
The v0.4.0 build was stopped before final release publication; do not treat
its tag or intermediate container uploads as a verified official release.

Grouped writes now release their worker, request-slot and retained-input
permits before delivering the completed reply. An immediately following write
can reuse a single admission slot, and metrics no longer observe that completed
request as admitted after its acknowledgement. Filesystem sync still precedes
successful replies; atomic frames and recovery semantics are unchanged.
A 64-write single-slot regression covers admission reclamation on every reply.

Back up and verify checksums before upgrading. Existing V3/V4/V5 files require
no migration. Review the snapshot memory trade-offs, optional group-commit
limits and rollback instructions in the 0.4.0 guide. Unfinished property-graph
development remains excluded. GitHub assets, container verification and PyPI
publication are separate stages; verify each destination before installing.
