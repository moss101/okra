# Decision records — okra

deepseek-harness pattern (MASTER-PLAN §3 #62): every load-bearing decision gets
a numbered note with lifecycle `proposed → implemented → rejected|archived`.
Lifecycle is CI-checked by `scripts/check-notes.sh`: a note in `implemented/`
must carry `Status: implemented` and an evidence line; `rejected|archived`
must carry a `Superseded-by`/`Because` line.

- [N0001 — Sync-first core, no tokio in M0 crates](0001-sync-first-core.md)
- [N0002 — Product name "okra", crate namespace `okra-*`](0002-namespace-okra.md)
- [N0003 — Protocol port scope: core delta ops; workflowRun.* deferred to M3](0003-protocol-port-scope.md)
- [N0004 — grok vendoring deferred; donor contracts re-implemented with citations](0004-vendoring-deferred.md)
