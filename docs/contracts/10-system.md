# System contracts

Status: current-contract
Owns: trust boundaries, byte boundaries, provider authority, auth, credentials, and sandbox claims.

## Read when

Read this before touching host/provider trust, callout authority, capabilities, auth metadata, credential storage, OAuth plumbing, sandbox docs, or any claim about what the host or provider may know.

## Rules

### Trust boundary

The host owns trust. Providers are untrusted WASM components. Filesystems expose one trusted host tree to the OS. Upstreams are external systems whose bytes and metadata must be treated as provider input.

Keep credential storage, credential injection, callout execution, cache storage, namespace state, and I/O in the host. Keep provider meaning in the provider: path meaning, object identity, canonical assembly, render, versioning, preload, and revalidation.

Host-owned blob and projection identities come only from validated mount-scoped
request facts. Providers may request a remote, reference, or HTTP payload, but
they never choose a filesystem/cache entry name, and injected credentials are
excluded from those identities. [Blob fetches](#blob-fetches) owns the facts of
a blob request and their validation. Mount definitions, credentials, provider
artifacts, SQLite state, and cache storage belong to the daemon; the CLI reaches
them only through typed local RPC.

### Byte boundary

The host operates on paths, bytes, content types, file attributes, cache metadata, capability outcomes, and effects. Object meaning stays provider-side.

Lower provider output into neutral host/tree types before filesystem adaptation. Keep canonical bytes opaque to the host. Do not decode canonical object payloads host-side to make projection decisions.

### Provider authority gates

New provider authority is a gated decision. Gate new callout families, new preopens, process effects, socket effects, broader network authority, and auth or transport changes. Describe the security model change and add enforcement-boundary tests in the same change.

Async host imports do not reduce this gate. A provider may suspend on a host import, but the host still owns execution, auth injection, capability checks, timeout behavior, and error mapping. Adding or widening an import is an authority change even when the SDK call site looks like ordinary async Rust.

Provider manifest `capabilities` declare authority needs only: domains, git repos, unix sockets, and preopened paths. Scalar resource ceilings such as memory and blob byte budgets are manifest `limits` and mount-spec `limits`; they must not be described as provider authority or callout grants.

Dynamic domain needs resolve from a provider config field named `domains`, whose string array becomes the mount's concrete HTTP allowlist at startup. Do not use a wildcard domain grant to stand in for this per-mount enumeration.

### HTTP transport

The host sends each provider HTTP request, inline or of a blob, through one transport (`HttpStack`): it approves the URL against the authority of the mount, injects the credential that the mount declares for that URL after the request left the provider, and returns the answer of any status. A successful status is `2xx` (`successful` in the host, `is_success` in the SDK, the one owner each): the body of each other answer is no content of a file, and the checked terminals of the SDK take each other status as an error, but the `304` that answers a conditional inline request.

Redirects are a gated transport decision, and this is the whole of it. The answer of a redirect status is the answer of the request, unless the request asks the host to follow it (`follow-redirects` of the WIT request, `follow_redirects()` of the SDK builders). The host then follows each `301`, `302`, `303`, `307`, and `308` with a `Location`, under these rules:

- Only a `GET` or a `HEAD` over HTTPS follows redirects: the host refuses another request that asks for it, before it goes out. A hop is to an HTTPS URL alone, so no redirect leaves HTTPS and none reaches a Unix socket.
- The authority of the mount approves the URL of each hop, as it approves the URL of a request.
- The host follows at most `MAX_REDIRECTS` hops for one request.
- A hop that the host refuses, and one too many, is the `denied` error of the request, which is not retryable and names the host of the hop. A redirect that the request asked to follow is never its answer.
- Each hop has the credential that the mount declares for its own URL, and none for a URL without one. A credential that a hop rejects is refreshed for that hop.
- The headers and the body that the provider wrote go to the origin of the request alone: the same scheme, host, and port. A hop to another origin has none of them.
- The answer names the URL that answered when it is another than the one of the request (`redirected` of the WIT response and of the blob answer, `url()` and `redirected()` of the SDK). The provider decides what the answer of another URL means, and the rate-limit breaker of the SDK records an answer for the authority that answered it.

The timeout of an inline request bounds the whole of it, redirects included.

### Blob fetches

A blob fetch is an HTTP request whose body stays in the host. The host validates its facts and derives its identity from them alone: the credential partition, the method, the canonical URL, the headers and the body of the provider, whether it follows redirects, `md5`, and `version`. `md5` is 32 lowercase hexadecimal digits, the MD5 of the bytes that the provider asks for, and `version` is 1 to 256 bytes without a control character, the version of those bytes that the provider read from its upstream: a statement of the provider that the host compares for equality alone. The host refuses a request whose `md5` or `version` is not valid before it asks the upstream.

The host stores the body of a successful answer alone, in the content-addressed body store, and hashes it once, as it arrives. With `md5`, other bytes are the `version-mismatch` error of the fetch, which is not retryable, and store nothing. The answer of another status has no body in the host and answers no other request.

This is the one keep rule. The answer that the host keeps is of the request, not of the operation: the host commits it at the end of the fetch, whatever its operation does after.

- A request with `md5` or `version` names the identity of its bytes, so it has one answer: the host keeps each successful one and answers the request from its projection from then on, without the upstream.
- A request without either has no identity of its bytes. For a `GET` whose provider wrote no condition and no range, the host keeps a `200` answer that has an `ETag` or a `Last-Modified`, and asks the URL that answered again with them as `If-None-Match` and `If-Modified-Since`: `304` is the kept answer, and another answer replaces it. That is one request without a body in place of a download.
- The host asks each other request each time, and its body is of its operation alone.

The answer tells whether the upstream answered this fetch (`upstream`): an answer from the projection says nothing of the upstream now, so the breaker of the SDK ignores it.

A blob handle is of the operation of its fetch alone, and holds its body in the store until the end of that operation. A body leaves the store with the collection after no fact of a projection refers to it and no hold holds it. The host cannot know whether a provider asks a request again, so the files bound the kept answers: an answer leaves at the second collection in a row that finds no file with its body. A collection runs at the start of the store and once the store took as many bytes as the collection before left, never for an operation; `crates/omnifs-engine/src/cache/collect.rs` owns its soundness.

These bound a fetch: the connect timeout, the idle bound of two minutes for the status of each hop and for each next bytes of the body, `MAX_REDIRECTS`, the `fetch_blob_bytes` limit that the mount declares, and the disk. Nothing bounds the whole time or the size of a fetch of a mount without a declared limit, by design: a bound of the whole time is a bound of the size at a given rate. An upstream that sends a byte within each idle bound holds a fetch open. Only a declared number can close that, such as a least rate in the limits of a manifest: the host has no number of its own that is right for each upstream.

The host runs the disk work and the digests of a fetch off the thread of the provider instance.

### Auth and credentials

Credentials live host-side in daemon-owned SQLite state. Startup resolves each
mount auth declaration into one mount-owned binding before namespace publication;
that binding loads the store entry, refreshes OAuth credentials during use, and
injects them after a callout crosses the WASM boundary.

Credential material stays out of WIT payloads. It may cross the daemon control boundary only in request-side protobuf messages on the local Unix socket; it never crosses filesystem attach/TCP or appears in responses, status, inventory, logs, Debug, or Inspector output. Route provider auth declarations through provider metadata and mount auth/config resolution. Keep human auth UX in `omnifs-cli`.

OAuth client ids in provider declarations are public application identifiers, not secrets. User access tokens, refresh tokens, and client secrets remain sensitive host-side values. `omnifs mount add` owns first-run OAuth mount generation; `omnifs mount reauth <mount>` owns repair and re-authentication; both submit the collected credential as one op in a lease-scoped mutation batch to the daemon. Credentials live only in the daemon credential store: a mount's auth declares its stored credential (scheme, account) and never a sourcing mechanism, so there is no read-from-env or read-from-file path at serve time.

The account of a mount is the label of its stored credential: `provider:scheme:account` is the address of one credential, mounts of one provider with different credentials have different accounts, and the host asserts an account to an upstream only as the subject of the assertion of a JWT bearer grant. `omnifs mount add` stores a credential of its invocation for its account alone, whatever way the invocation gives it: a token that it reads, a token that it imports from an ambient source, or an OAuth client. It always submits that credential and replaces the stored one of the account with it, and refuses the account whose credential another mount holds, which `omnifs mount reauth` replaces. A stored credential stands only for an invocation without one.

`omnifs mount reauth` keeps the scopes of the stored credential unless its invocation gives others; without a stored credential, as after `omnifs credential rm`, it requests the scopes that the manifest of the provider declares.

A command refuses each flag that the auth of its mount cannot use, in place of leaving it out: `--account`, `--client-id`, `--client-secret-file` and `--scope` for a provider without auth; `--client-id`, `--client-secret-file` and `--scope` for a static token; and `--token` and `--token-env` at `omnifs mount reauth` of an OAuth scheme.

The OAuth JWT bearer grant is the one of RFC 7523. The host signs an assertion with the client ID as `iss`, the token endpoint as `aud`, and as `sub` the account of the credential that the client acts as, as the user whose data a service account with delegation reads: an assertion for the default account names no `sub`, the form that the authorization server of Google takes for a service account itself. `iat` is the time of the signature and `exp` an hour after it, the longest that the authorization server of Google takes; the host adds no allowance of its own for the clocks. The host sends the scopes as the `scope` parameter of the token request. What an authorization server takes otherwise is a declaration of the manifest of the provider, never a profile in the host: the flow `jwtBearerScopeClaim` names the claim of the assertion that has the scopes in place of that parameter, and that claim is none of the registered claims of a JWT (`iss`, `sub`, `aud`, `exp`, `nbf`, `iat`, `jti` of RFC 7519, and `prn`). A JWT bearer scheme declares no authorization endpoint and no token endpoint auth: manifest validation refuses either. The manifest of an artifact of an earlier omnifs, whose flow `jwtBearer` had the scopes in the claim `scope` and had to declare an authorization endpoint, is therefore invalid, and a mount on that artifact is `ProviderUnavailable` with that error until `omnifs mount update --provider` gives it a current artifact and `omnifs mount reauth` a credential. A token of an answer without `expires_in` has no known expiry and serves until the upstream refuses it; an `expires_in` past the last time is an error of the answer.

The validation of a static token is a declaration of the manifest that binds its probe: one request of the declared method to the declared URL, with the token in the header of its scheme and each declared header beside it, which follows no redirect, so that the token reaches the declared URL alone and a redirect is a failed validation. The token is valid with the declared status, which is no redirect, and with a value at the declared JSON pointer that is not `null`, `false`, or an empty string.

The fingerprint of a credential is of its provider, its auth scheme, and the client ID, the redirect URI and the scopes of its client, without its client secret. A credential with the fingerprint that an earlier omnifs gave it stays bound to its mount, and its next submission stores the fingerprint and the encoding of this one.

A credential that its upstream rejects, by a 401 or by a 403 that names an invalid bearer token, requires re-authentication in the health of its mount from that answer on, up to an answer in which the upstream takes it: a static token, which has no refresh, and an OAuth token whose refresh fails. `omnifs status` gives that mount with the auth state `error` and its `omnifs mount reauth` command, and ends as degraded.

Credential values must never appear in CLI output, errors, tracing, metrics, or structured envelopes. Source identifiers such as environment-variable names may appear when they make an error actionable.

### Filesystem attach authority

The Docker-hosted filesystem receives no credentials or host filesystem mounts. Its only host authority is the Omnifs VFS wire protocol over a local TCP attach. Docker Desktop reaches a loopback listener through its host forwarder; native Linux reaches a listener bound specifically to the address assigned to the default `docker0` bridge. The daemon validates that interface assignment rather than trusting a caller-supplied address. Do not bind the attach listener on every host interface or give the filesystem host networking merely to cross the container boundary.

The libkrun filesystem guest also receives no credentials or network device. Its only host authority is the three fixed vsock paths for attach, readiness, and keyed ssh. The trusted, signed `omnifs-libkrun` helper owns Hypervisor.framework and libkrun calls; the guest and provider WASM never gain that host authority.

## Must not

- Put provider-specific behavior in `omnifs-engine`, `omnifs-fuse`, or `omnifs-nfs`.
- Claim the sandbox prevents all exfiltration. Allowed network destinations can still be abused by a hostile provider.
- Add provider authority as a side effect of a convenience change.
- Hide a new capability behind a macro argument, manifest field, or config field that is not enforced.
- Transmit credentials through filesystem attach/TCP or expose them in daemon responses, status, inventory, logs, Debug, or Inspector output. Request-side submission on the local Unix control socket is the only allowed crossing.
- Let providers read the credential store directly.
- Build a provider-specific credential bypass in host runtime code.
- Give the host the profile of a vendor for an OAuth grant.
- Treat WIT async imports as provider-owned I/O.

## Code

- `crates/omnifs-wit/wit/provider.wit`
- `crates/omnifs-engine`
- `crates/omnifs-engine/src/callouts/mod.rs`
- `crates/omnifs-engine/src/callouts/http.rs`
- `crates/omnifs-auth`
- `crates/omnifs-state/src/credential.rs`
- `crates/omnifs-provider/src/manifest.rs`
- `crates/omnifs-cli/src/commands/mount/`
- `crates/omnifs-daemon/src/generation_builder.rs`
- `crates/omnifs-cli/src/commands/mount/`
- `providers/*/README.md`

## Validation

- For authority or callout changes, run `just build providers` and host tests that initialize providers.
- For auth changes, test status/readiness output, credential resolution, and the callout path that receives injected auth.
- For WIT or cache boundary changes, add a WIT-boundary or host integration test that asserts lowered bytes, attrs, and effects without provider-specific host decoding.
