# HP-1 ownership and WIP status

This session claims only `mahpate-hp1-gap6-write-oracle`, based on
`136d72c5a94e1c0d1878f99c461a555a91b7c3f6`. It will not start HP-2, HP-3
or HP-4 or integrate any other machine's work.

Remote discovery also shows the HP-2 and HP-4 package branches at the base.
No HP-3 branch was visible at the last check; this is not evidence that HP-3
is unassigned. The operator reports three other machines working in parallel.
Cross-machine sessions are not available through this host's session list.
The published branch is a visible ownership claim, not an acknowledged
assignment by every peer. Confirmation was requested but the user was
unavailable; implementation is paused rather than assuming exclusivity.

The package is incomplete. Only investigation reports exist. There is no
write oracle, fixture corpus or mutation campaign, and no completion claim.
The initial byte-contract stop was based on an overly narrow reading of the
handover's source list; the follow-up correction in
`tests/graph_subset_write_report.md` supersedes that conclusion.

To resume: confirm HP-1 has no other owner, enumerate the existing general
graph codec and adapter byte rules without reading the forbidden subset
implementation, then implement the package and run its specified checks.
Do not treat existing read-oracle passes as write coverage.
