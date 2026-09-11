# Rulings taken on your behalf — OAuth authorization branch

Every decision the controller made without asking, in the order it was made.
Each one states what it costs if it is wrong. Rework whatever you disagree with.

The five pre-flight rulings came from a conflict scan of the plan before any code
was written. The rest came from the review loop.

1. Ruling R1: `Config::bearer_scopes` stays `Vec<ToolCategory>`, and `HttpState` converts with `Arc::from`. Reason: `Config` is `Clone` and plain data; `HttpState` is cloned per request, where `Arc<[T]>` is cheaper. Cost if wrong: one type change in two files.

2. Ruling R2: Task 4 declares `OauthState` with every field that later tasks need, including the injectable clock. Reason: retrofitting the struct in Task 8 would edit code that a task review already approved. Cost if wrong: one unused field until Task 9.

3. Ruling R3: `tests/support/flow.rs` is created by Task 7 and extended by Tasks 8 and 9. Reason: Task 7 is the first task that needs to drive a client to the consent page. Cost if wrong: one file moves one task earlier.

4. Ruling R4: Task 1 uses the real constructor `GraphHandle::new`, copied from `tests/mutation_service.rs:14`, through a local `test_graph` helper. Reason: the plan invented `new_for_test`, which does not exist. Cost if wrong: none; the helper is test-local.

5. Ruling R5: the dependency constraint now names one new lockfile entry, `jsonwebtoken`, and tells the implementer to verify with `grep` before adding a crate. Reason: the original line was false and would have blocked a correct `http-body-util` dev-dependency. Cost if wrong: an implementer adds a crate that was already present, which the lockfile diff shows.

6. Ruling R6: implementers never run git, and the controller commits every task. Reason: the skill's implementer template tells the subagent to commit, and the project rule in `~/.omp/agent/rules/orchestration.md` forbids a subagent from running git. The project rule wins. Cost if wrong: the controller writes each commit message instead of the implementer, which is where the cost and the token accounting belong anyway.

7. Ruling R7: implementers run one at a time, in the main checkout, on branch `feature/oauth-authorization`. Reason: the skill forbids parallel implementers, so the three worktrees offered in conversation would have stood idle. Cost if wrong: wall-clock time on tasks 1, 2 and 3, which have disjoint file scopes.

8. Ruling R8: the `tools/call` gate keys on `scope_of`, so an unknown tool name keeps its `-32601 Method not found`. Reason: the plan's snippet used `unwrap_or("graph-read")`, which turns every unknown name into `-32002` for every caller, including today's full-scope caller. That changes existing behaviour and states a false reason. The spec forbids a behaviour change for existing users. Cost if wrong: an unknown tool reports the wrong error code, which a test pins.

9. Ruling R9: the error variant lives in `crates/mcpmem-core/src/errors.rs`, not in `src/errors.rs`. Reason: `src/lib.rs:10` re-exports `mcpmem_core::errors` and declares no `mod errors`, so `src/errors.rs` is dead and is never compiled. Cost if wrong: none; the dead file is unchanged.

10. Ruling R10: finding 3, the notification screen, is promoted out of the deferred set into fix round 1. Reason: the skill keeps Minors out of the loop. But Task 8 introduces principals that can be denied. The finding makes a whole batch answer 403 for a message that would never have executed. The fix is one line. Cost if wrong: one line and one test that the final review would have asked for anyway.

11. Ruling R11: the Task 2 role-refusal test uses role `webhooks`, gated with `#[cfg(feature = "webhooks")]`, and runs with `--features webhooks`. Reason: the plan wrote `webhook-worker`, which is not a role name, and `webhooks` is not a default feature. Therefore `RoleSet::parse_csv` would reject it for the wrong reason. Cost if wrong: the test needs a feature flag on its command line.

12. Ruling R12: the two P3 residuals from the re-review join fix round 2. Reason: the round is happening anyway for the Important finding. Both are one-line changes. The `"id":null` case is the round-1 defect in a narrower trigger. Cost if wrong: two extra lines and one extra test case.

13. Ruling R13: `--public-url` may carry a path. Reject only a query string and a fragment. Reason: the MCP authorization specification names `https://mcp.example.com/server/mcp` as a valid canonical resource URI. A deployment behind a path prefix is therefore legal, and the reviewer's rule would forbid it. Cost if wrong: an operator can set a public URL with a path that the reverse proxy does not serve. The connector reports that failure at discovery time.

14. Ruling R14: the two test-integrity findings from the re-review join fix round 2, although they are Minor. Reason: one is an assertion that cannot go red. The other is a guard clause with no test. Both weaken the evidence this process depends on. The round is happening anyway for the Important finding. Cost if wrong: two small test edits.

15. Ruling R15: `oauth_code` gains a `family` column. Reason: `CodeGrant` holds a full `Grant`, which carries `family`, and the brief's own test compares the whole `CodeGrant` after `take_code`. Without the column the family cannot round-trip. Migration 4 is new in this branch and has never been applied, so no checksum breaks. Cost if wrong: one column that nothing reads.

16. Ruling R16: `sweep` clears the three tables that carry an expiry. Reason: the brief said all four, and `oauth_client` has no `expires_us`. A client is evicted by its last-used time, which is Task 9 work. Cost if wrong: unused client rows survive until Task 9 lands.

17. Ruling R17: every Minor from the Task 3 review joins the fix round. Reason: finding 8 closes a gap the task contract calls Critical, because an edit to migration 3 is caught by no test today. Findings 3 and 5 are security-shaped and one line each. Finding 4 is a false claim in a module doc comment. Finding 7 removes a duplicated hash helper and makes an unused dependency true. The rest are one-line test additions. Cost if wrong: a larger fix diff for one round.

18. Ruling R18: finding 1 takes the doc-comment precondition, not the signature change. Reason: it keeps the `Store::new` interface the brief pinned, and Task 4 owns the connection that must carry the busy timeout. Cost if wrong: Task 4 can forget the timeout, and a concurrent refresh replay then reports an error instead of revoking the family.

19. Ruling R19: `family_of` gains `now_us` and filters on expiry. Reason: Task 4's revocation endpoint calls it. Without the filter, a holder of a long-expired refresh token can revoke the live family for that principal. It stays unfiltered on spent and revoked, because revocation must be idempotent. Cost if wrong: an expired token cannot revoke a family it no longer belongs to.

20. Ruling R20: `http::run` takes one parameter struct instead of eight positional parameters. Reason: `bearer_scopes` and the `enabled_categories` that Task 4 adds are both `Arc<[ToolCategory]>` and adjacent. A silent argument swap is therefore one edit away, and the compiler cannot catch it. Cost if wrong: one struct definition and one call site.

21. Ruling R21: register both protected-resource routes, and derive the `resource` value from the request path. Reason: RFC 9728 section 3.1 inserts the well-known segment between the host and the path. A client that starts from `https://host/mcp` therefore asks for the suffixed URL. Section 3.3 then requires the returned `resource` to equal the identifier the client used, so one constant document cannot serve both callers. Cost if wrong: one extra route that no client asks for.

22. Ruling R22: every Minor from the Task 4 review joins the fix round. Reason: three of them close evidence gaps, and the other three are one-line changes in code the round already touches. Cost if wrong: a larger fix diff for one round.

23. Ruling R23: register the suffixed authorization-server route too, with the same identical-or-404 rule. Reason: with `--public-url https://host/base`, the protected-resource document names the authorization server `https://host/base`. RFC 8414 section 3.1 then sends the client to `https://host/.well-known/oauth-authorization-server/base`, which answers 404. The flow dead-ends one hop after discovery succeeded. Cost if wrong: one route that no client asks for.

24. Ruling R24: remove the trailing-slash trim from the suffix comparison. Reason: with it, `/.well-known/oauth-protected-resource/mcp/` answers 200 with a `resource` value that is not what the client used. A validating client then aborts on a 200 from a handler whose doc comment claims section 3.3 compliance. One rule covers every suffix: identical, or 404. Cost if wrong: a client that appends a trailing slash gets 404 instead of a document it would reject anyway.

25. Ruling R25: rounds 4 and 5 kept the same implementer instead of escalating to a fresh one on a stronger model. Reason: the skill escalates because a loop that survives three rounds usually means the implementer cannot see its own problem. That was not the case. Every round closed every finding sent, and the residuals were new observations, two of which were the controller's own stale plan text. Cost if wrong: one more round with an agent that already held the context.

26. Ruling R26: Task 5 adds no dependency. It ships the `Fetch` trait and `resolve_metadata_document` with no network code. Task 6 owns `reqwest`, the real fetcher, and the fix for the CI guard. Reason: the guard at `.github/workflows/ci.yml:59` fails when `cargo tree --no-default-features -e normal` names `reqwest`, and `mcpmem` depends on `mcpmem-oauth` with no feature gate. Nothing in Task 5 makes a network call, so a dependency added here would be unused and would break a green guard for no gain. Cost if wrong: Task 6 carries one more file. Carry into Task 6: the guard is a real requirement. A graph-only build must hold no HTTP client. Task 6 must put `reqwest` behind a cargo feature. It must not widen the guard. The plan's Task 6 file list adds `reqwest`, `jsonwebtoken` and `url` to `crates/mcpmem-oauth/Cargo.toml` with no feature gate. The plan is wrong here. The controller will correct it before Task 6 is dispatched.

27. Ruling R27: all seven Minor findings join one fix round. Reason: finding 1 lets one unauthenticated request write a row of up to 16 MiB. That endpoint is open by design. Finding 3 lets a redirect URI carrying a fragment through registration. That URI surfaces in Task 7 as a dead flow rather than a refusal. The rest are one-line changes in the same files. Cost if wrong: a larger fix diff for one round.

28. Ruling R28: delete `Store::set_login_principal` and its test now, rather than leaving it for Task 7. Reason: an uncalled method with a passing test is code a later reader assumes is load-bearing. Cost if wrong: Task 7 adds it back with a caller.

29. Ruling R29: add the `azp` check. Reason: `jsonwebtoken` tests the audience as a non-empty intersection, so a token naming this client and another passes. OpenID Connect Core section 3.1.3.7 requires `azp` to equal the client identifier in exactly that case. Cost if wrong: a provider that omits `azp` on a single-audience token is unaffected. The check applies only when the audience names more than one party.

30. Correction to ruling R29: the ruling cited a MUST on `azp` that OpenID Connect Core does not carry. Section 3.1.3.7 of errata set 2 makes `azp` a SHOULD. The MUST is item 3. It says to reject a token that does not name this client as an audience. It also says to reject a token that carries an audience this client does not trust. The `azp` check was therefore correct but partial, and it was justified by the wrong sentence. The implementer repeated the wrong citation in good faith, because the controller wrote it.

31. Ruling R30: refuse an identity token that names any audience other than the configured client. Reason: `jsonwebtoken` tests the audience as a non-empty intersection. A token naming this client and an attacker's client therefore passes both the audience check and the `azp` check when no `azp` is present. The nonce binding blocks the attack today, so this is hardening, not a live hole. Cost if wrong: a provider that mints multi-audience identity tokens on purpose needs a configuration option this server does not have. Also corrected: the implementer reported that `aud` cannot be recovered because `jsonwebtoken` consumes it. That is false. `jsonwebtoken` 9.3.1 deserializes the payload twice, so the claim type can carry `aud`.

32. Ruling R31: refuse an authorization request that names no scope, at `/oauth/authorize`, before the human is sent anywhere. Reason: the empty intersection at the callback must stay a 403 with no detail, because disclosing it would answer a question about a named human. A request that names no scope at all discloses nothing and is knowable before the redirect, so spending a human sign-in on it is waste. Cost if wrong: a client that omits an optional parameter gets a 400 it can read, instead of a silent dead flow.

33. Ruling R32: `tests/support/flow.rs` gains a helper that walks to an authorization code without spending it. Reason: Task 8 starts from a code. A method named `take_code` that spends the code is the wrong shape for the task that must exchange it. Cost if wrong: one helper Task 8 does not call.

34. Ruling R33: every Minor joins the fix round. Reason: three of them shape the fixture Task 8 starts from, and Task 8 is next. Cost if wrong: a larger fix diff for one round.

35. Ruling R34: close the unnamed-client class rather than the next input. Reason: round 1 refused an empty client name, and round 2 refused a blank one. The re-review then found that `trim` leaves U+200B and U+202E in place. The page therefore draws nothing for the third time. A third narrower condition would invite a fourth round. The property is "the name carries a rendering character". U+202E is also removed in `escape_html`. This is because a name that reverses the sentence beside it spoofs the one page whose job is to inform a human. Cost if wrong: a client whose name is entirely non-rendering is shown by its identifier.

36. Ruling R35: rounds 3 and 4 kept the same implementer. Reason: each round closed everything sent, and the reviewer kept finding a narrower input rather than the same one twice. The final fix was fully specified by the reviewer, so a fresh agent would have added cost and no judgment. Cost if wrong: one more round with an agent that already held the context.

37. Ruling R36: a replayed authorization code revokes the family it already produced. Reason: RFC 6749 section 4.1.2 asks for it, and the refresh path already treats two parties presenting one single-use credential as fatal. Today the code path cannot revoke, because `take_code` deletes the row. The fix is a `spent` column on `oauth_code`, in migration `0004_oauth.sql`, which is new in this branch and has never been applied anywhere. That reuses ruling R15. Cost if wrong: one column and a row that the sweep already clears.

38. Ruling R37: the viewer gate routes through `authz`, not a hand-spelled scope check. Reason: `src/authz.rs` is documented as the one place the scope decision is made, and Task 1 spent two rounds making that true. Cost if wrong: one call site changes shape.

39. Ruling R38: the 429 body keeps `temporarily_unavailable`. Reason: the reviewer ruled the concern with an argument the controller accepts. A 429 is an HTTP-level refusal. RFC 6749 section 5.2 governs the 400-class token errors and does not reach it. `slow_down` belongs to device-code polling. Cost if wrong: a connector that branches on the body sees an authorization-endpoint code.

40. Ruling R39: wire Client ID Metadata Documents rather than retract the advertisement. Reason: the discovery document says `client_id_metadata_document_supported: true`, and no code path implements it. `resolve_metadata_document` has no caller, no `Fetch` implementation ships, and `--cimd-allowed-domain` is parsed and never read. The resolver, the allow-list, the caps and their tests exist and are reviewed; only a fetcher and one call site are missing. Retracting would delete reviewed code and drop a capability the design chose. Cost if wrong: one more fix round. This gap is the controller's fault. Ruling R26 moved the real fetcher from Task 5 to Task 6, and no later brief then wired it into the authorize endpoint. The plan never held a step that did so.

41. Ruling R40: name the grant's destination on the consent page. Reason: registration is open and the client name is attacker-chosen. A human must therefore see where the grant is delivered, and not only who claims to ask. Cost if wrong: one more line on the page.

42. Ruling R41: refuse `--oidc-issuer` at startup when the `oauth` feature is off. Reason: serving half an authorization server is worse than refusing to start. A connector discovers the server and then fails at the login hop with no explanation. Cost if wrong: a graph-only build rejects a flag it used to ignore. Final fix wave dispatched — 9 findings. FIX_BASE `36fcb94`. One wave, then one scoped re-review, then the human.