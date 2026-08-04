# 2026-07-28 finalization: game plan

Untracked working doc. The `2026-07-28` protocol **finalized on 2026-07-28**.
Scope was: verify and decide, not implement parity. Full parity (#950 MRTR,
#951 tasks, #952 subscriptions/listen, #953 client) is the multi-week roadmap
that follows.

## 1. Gate check -- DONE

- [x] Confirmed finalized. Tag `2026-07-28` cut 16:47 UTC, release published.
      `schema/2026-07-28/` and `docs/specification/2026-07-28/` exist.
      `schema/draft/` still reads `LATEST_PROTOCOL_VERSION = "2026-07-28"`, so
      draft has not advanced. rmcp cut `rmcp-v3.0.0` stable the same day.
- [x] Pulled the final schema.
- [x] Harness: still `0.2.0-alpha.10` (2026-07-27). npm `latest` is `0.1.16`.
      No stable 0.2.0 line yet.
- [x] Diffed RC against final: 578 lines, most already absorbed. The
      `-32020`/`-32021`/`-32022` renumbering landed here in June via spec PR
      #2907. Unfiled deltas are now **#1037**.

Caveat: the upstream versioning page still says current is `2025-11-25`. It is
a lagging doc, not a signal.

## 2. Verify -- server half DONE

- [x] Server suite on `alpha.10`, `--spec-version 2026-07-28 --suite all`:
      **67 passed / 34 failed** (101 checks), versus baselined 66/102 on
      `alpha.9`. **No regressions from finalization.**
- [x] The 12 failing scenarios match `conformance-baseline-draft.yml` exactly.
      Two baselined entries now pass and are stale:
      `input-required-result-missing-input-response` and
      `input-required-result-ignore-extra-params`.
- [ ] Client suite on `alpha.10` -- not yet run. Needed for the promotion call.
- [x] Stateless core holds against final.

## 3. Decide -- still open

- [ ] **`SUPPORTED_PROTOCOL_VERSIONS` promotion.** Wants the re-baselined
      numbers from #1038 plus the client-suite half. Criterion unchanged: is
      the stateless core conformant with bounded, tracked gaps?
      Current read leans yes; `server-stateless` is the one core scenario still
      failing, and it is scoped in #952.
- [ ] **1.0 posture.** Likely stay pre-1.0 so breaking changes can land without
      a major bump. Confirm.

## 4. Filed, ready to execute

| Issue | Work |
|---|---|
| **#1037** | RC-to-final schema deltas. `DiscoverResult` drops `serverInfo` and extends `CacheableResult` (breaking); server identity in result `_meta`; version-gate the elicitation removals; smaller narrowings |
| **#1038** | Re-baseline to harness `alpha.10`: pin bump, drop the 2 stale entries, re-run the client half, update README/CLAUDE.md figures |
| #950 | MRTR (SEP-2322). 11 of the 12 remaining server failures |
| #951 | Tasks extension (SEP-2663) |
| #952 | Listen semantics, incl. the `subscriptionId` tagging and teardown from the final diff. Owns `server-stateless` and `http-custom-header-server-validation` |
| #953 | Client parity |

Start with #1038 (cheap, unblocks the promotion decision), then #1037 part A
(the one breaking change), then #950 for the bulk of the conformance delta.

## 5. Do not

- [ ] Do not promote blindly until the client suite is re-run.
- [ ] #1027 / #1028 / #1029 stay backlog.
