# Loopback redirect URI port matching — decision proposal

Date: 2026-09-12. Status: **proposal, awaiting owner decision**. Not an
accepted plan. Not committed.

## The problem

The OpenAI Codex connector presents a redirect URI of the form
`http://127.0.0.1:<port>/callback`. The port is fresh on each run. Its
metadata document, `https://chatgpt.com/oauth/codex/client.json`, declares:

```json
"redirect_uris": ["http://127.0.0.1/callback", "http://localhost/callback"]
```

No port. This server compares every redirect URI byte for byte
(`src/oauth_routes.rs`, `start_login`). The comparison can therefore never
succeed for the CIMD row, and every Codex login through the metadata-document
path answers `redirect_uri is not registered for this client`.

Dynamic registration (DCR) works, because the client registers the exact live
port one request before the authorization request. But the connector's `Auto`
registration setting selects the metadata-document path, and an operator
cannot change what OpenAI serves in its document.

The codebase records the byte-exact decision and its price in
`crates/mcpmem-oauth/src/registration.rs`
(`is_acceptable_redirect_uri`): "Do not relax the comparison to 'comply with
7.3' without revisiting this: it is a decision, not an oversight." This
document revisits it.

## What the standards say

RFC 8252, section 7.3 (verbatim):

> The authorization server MUST allow any port to be specified at the time of
> the request for loopback IP redirect URIs, to accommodate clients that
> obtain an available ephemeral port from the operating system at the time of
> the request.

Section 8.1 bounds the threat model (verbatim):

> The redirect URI options documented in Section 7 share the benefit that only
> a native app on the same device or the app's own website can receive the
> authorization code, which limits the attack surface. However, code
> interception by a different native app running on the same device may be
> possible.

> The PKCE [RFC7636] protocol was created specifically to mitigate this
> attack. ... An app that intercepted the authorization code would not be in
> possession of this secret, rendering the code useless.

The MCP authorization specification states the same comparison rule for
loopback redirects: the scheme, host and path must match exactly, and the port
is the sole exception.

## The proposal

Add one predicate, `redirect_uri_matches`, and use it in place of the exact
`contains` check in `start_login`. The predicate accepts when:

1. The two strings are equal. This keeps every current behavior.
2. Otherwise, all of the following hold:
   - the registered URI is `http` or `https` to a loopback host: `127.0.0.1`,
     `[::1]`, or `localhost`;
   - the registered URI carries **no explicit port**;
   - the presented URI names the **same loopback alias**, exactly;
   - scheme, path and query of the two URIs are equal;
   - the presented port is ignored.

Every other difference — a different alias, a non-loopback host, a different
scheme, a different path, a registered URI that **does** carry a port — keeps
the byte-exact rule.

Why the "no explicit port" condition matters: a dynamic registration made one
request before authorizing **can** record the exact live port, and the existing
test `a_loopback_redirect_uri_differing_only_in_port_is_refused` pins that
behavior. That test stays green and keeps its meaning. The relaxation is
exactly as wide as the case that needs it: a fixed, vendor-published document
that cannot know the port.

RFC 8252's own examples always draw the port (`http://127.0.0.1:{port}/{path}`),
so the port-less loopback registration is OpenAI's extension of the standard.
A client that registers a specific port is declaring it wants that exact port;
refusing a different port for such a registration is stricter than the RFC's
minimum and costs nobody anything, because a DCR client can always re-register
per port.

## What stays exact

- The token endpoint (`crates/mcpmem-oauth/src/token.rs`). The code row stores
  the presented redirect URI, and the token request must echo it exactly. A
  well-behaved client sends the same string both times, so nothing changes.
- Non-loopback registrations, in every case.
- Loopback registrations that carry a port.
- The consent page. It draws the destination of the presented URI, which is
  where the client really returns to.
- Registration acceptability (`is_acceptable_redirect_uri`): loopback
  `http`, https, no fragment. Unchanged.

## Security analysis

The relaxation admits one new shape of attack. A hostile native app already
running on the human's device starts an authorization with a trusted client
identifier (for example `Codex`), uses its own loopback port and its own PKCE
pair, and if the human approves, receives the code and redeems it. The consent
page names `Codex` and the destination `127.0.0.1:<port>`, which is exactly
what the genuine flow shows at that moment, so the page cannot distinguish the
two.

Three facts bound this:

1. **The code never leaves the device.** Only a process already running on the
   machine can collect it. RFC 8252 section 8.1 states that this is the
   accepted baseline for loopback redirects: the attack surface is "a native
   app on the same device". A hostile local app has strictly easier attacks
   than this one — it can race the genuine client for its port, which is the
   interception scenario PKCE defeats.
2. **PKCE is mandatory here.** The server rejects an authorization request
   without `code_challenge_method=S256`. Intercepted codes are useless without
   the verifier.
3. **The human still approves.** The relaxed comparison changes what the page
   can say, not the fact that the human decides.

The comparison before this change protected a local channel against a remote
attacker. A remote attacker cannot read a loopback redirect; a local attacker
was already inside the threat model. That is why RFC 8252 makes port-agnostic
matching mandatory for this case.

## Alternatives considered

1. **Keep the current behavior; the connector must select DCR.** Zero code
   change, but a CIMD row for Codex can never succeed, the `Auto` default keeps
   failing, and every new OpenAI-native client with an ephemeral port hits the
   same wall.
2. **Port-agnostic matching for every loopback registration, including
   port-pinned DCR rows.** This is the RFC/SSL-literal reading. It flips the
   recorded test, and it weakens a registration that explicitly pinned a port,
   for no user story. Rejected.
3. **Also treat the aliases `localhost`, `127.0.0.1` and `[::1]` as
   equivalent.** Unnecessary: OpenAI registers all aliases it uses. Aliases
   stay distinct, which matches the existing registration rule ("the loopback
   names are matched whole"). Revisit only if a client registers one alias and
   presents another.
4. **Re-fetch the metadata document on every login.** Cannot help: the
   document cannot name the port.

## Blast radius

| Place | Change |
| --- | --- |
| `src/oauth_routes.rs` `start_login` | the `contains` comparison becomes the predicate |
| `crates/mcpmem-oauth/src/registration.rs` | add `redirect_uri_matches`; rewrite the "half of RFC 8252" comment to name the new boundary |
| `tests/oauth_consent.rs` | keep the port-pinned refusal test; add: port-less registered + any presented port is accepted; different alias is refused; different path is refused; non-loopback port change is refused |
| `tests/oauth_cimd.rs` | add a full CIMD loop: document declares port-less URI, presented URI carries a fresh port |
| Consent page, token endpoint, DCR, registration | unchanged |
| `docs/runbooks/oauth-deployment.md` | the "changed its port" section and the ChatGPT section gain the port-less exception |

## Decision

Three options, one recommendation:

- **A2 (recommended):** the narrow exception above — port-agnostic only for
  port-less loopback registrations. Solves Codex CIMD, keeps the recorded
  port-pin protection.
- **A1:** RFC-literal — port-agnostic for all loopback registrations. Flipping
  the recorded test.
- **None:** keep DCR as the only working path for ephemeral-port clients.

## Cost of this analysis

~10k tokens (estimate); ~$0.10 (estimate); one round-trip to the deployment
database and to RFC 8252.