# Projection tree contracts

Status: current-contract
Owns: shared projection semantics, file attributes, cache ownership, listing, lookup, learned sizes, and live-file behavior.

## Read when

Read this before touching `omnifs-engine/src/tree`, projected node resolution, cache access, attrs, listing, lookup, traversal, learned sizes, negative lookup behavior, live growth, or behavior shared by FUSE and NFS.

## Rules

### TreeNamespace owns projection semantics

`omnifs-engine::namespace::TreeNamespace` is the sole public semantic facade. Its internal tree implementation owns answers to what projected path exists, what bytes or attrs it has, what cache entry should be published, what root children exist, and what provider probe is needed.

Put shared projection semantics behind `TreeNamespace`. Move behavior out of FUSE and NFS when it becomes filesystem-neutral. Keep mount-root enumeration and host traversal in the namespace owner.

### File attributes

One file contract owns file truth per path. Size, stability, read mode, content type, byte source, and version evidence must stay together.

Learned sizes and read semantics belong with file attrs and tree policy, not filesystem-local heuristics. Preserve learned-size rules in shared code and test tools that observe attrs and reads differently.

An answer with the attributes of a file reads nothing: it tells the size of the version that the projection has, and the namespace keeps that version as the one that the reader was told. A reader takes as many bytes as the attributes that it has say, so a read that takes another version, as in a sync in which the file is due or through an answer of its feed, gives the bytes of the version that was told when the new one has another length, or when the read continues one that began, and the next answer with the attributes tells the new version; a version of the same length from its first byte is the version that the reader takes. A file whose length changes upstream therefore reads one version late unless its facts are fed: a feed keeps the size in the projection current without a read.

### Cache ownership

The host owns all caching as opaque byte storage. Providers do not add private LRUs or time-based expiration policy. Filesystems do not own cache schema.

The namespace boundary carries engine-issued cache lifetimes. A filesystem may retain only the plain positive or negative answer for that lifetime and must evict it on namespace invalidation. Missing children are lookup answers, while transport, offline, and provider failures remain errors.

`Caches` owns one Fjall database and one global content-addressed `BodyStore`. One `ProjectionStore` keyspace, selected by the number of the projection of the provider of a mount, its canonical mount without its provider artifact, and its credential generation, owns every durable fact for that immutable projection. The process-local memory tier is derived and belongs to the projection's `MountResources`.

Exactly one live `Arc<MountResources>` exists per projection identity in a process. Its single transition boundary publishes object relations, typed lookup/attr/file/listing facts, Git references, freshness, and invalidations in one durable transaction. A provider terminal is observable only after that transaction commits and the derived memory tier is invalidated. Runtime invalidation epochs remain process-local and fence the complete stale transition.

Online and cache-only serving use the same `TreeNamespace` and fixed `MountTable`. Cache-only entries have no provider runtime: a mount without its provider is one. Complete durable facts remain usable regardless of online freshness, and partial durable listings return their known entries with no provider-dependent continuation; missing bodies or facts still return terminal `OfflineMiss`, while corruption aborts construction of the whole table.

A sync of a mount is an explicit fact of the system: `omnifs sync begin MOUNT`, the `BeginSync` call of the control protocol, begins it and gives its number, the one after the last one begun, which the projection keeps over a restart; `omnifs sync end MOUNT N`, the `EndSync` call, ends it, and the begin of the next one and the end of the daemon end it too. Each feed, view leaf, page, and Git tree of the mount whose facts were written before the sync is due once in it. An operation outside a sync is served from the projection, except an operation on a fact that the projection lacks, which takes it, and an operation on a feed whose first answer has not ended, which continues it. Nothing of the host asks an upstream because time passed, and nothing of it watches one: no timer, event subscription, webhook, or notification asks a provider.

Access-driven revalidation is the cache freshness boundary. An expired indexed view leaf makes the next read enter `read-file` with the cached canonical id, validator, and bytes in `ReadMode::Revalidate`, so normal provider effects apply refreshed canonical bytes or invalidations. An indexed view leaf expires in a sync in which it is due, once, and serves outside a sync.

A cached listing keeps its pages, each with the cursor that asked for it. A page with a validator, complete or not, is due once in each sync after its answer, and not before the end of the interval that its upstream states after its answer, as `X-Poll-Interval`, which the host compares with the clock. Before a cached listing serves in a sync, each of its due pages is asked for again, in page order, with its cursor and validator: `unchanged` keeps the entries of the page with the validator of the answer, due in the next sync, a changed page replaces its entries, keeps the later pages only while its next cursor stays, and records the directory as changed, and a failed answer, a rate limit included, is the error of the listing. Every `list-children` call carries the validator of the cached page of its cursor. A page without a validator keeps the freshness of a cached listing: a complete or paged listing serves until it is invalidated. While the window that the upstream of the mount stated is open, a cached listing serves without asking.

A Git tree keeps the commit of its last handoff in the Git fact of its tree path, due once in each sync after each answer for it. Before the root of a tree or a node of it is served in a sync in which the tree is due, `list-children` of the tree path asks again with the commit as its validator, in the one flight of the path and not while the window that the upstream of the mount stated is open: `unchanged` makes it due in the next sync, and a subtree with a new commit updates the Git fact, makes it due in the next sync, and records the tree path as changed, so that filesystems see the attributes of the root of the tree changed in place. The nodes of a tree answer filesystems with the zero lifetime of dynamic entries, so each access reaches the namespace. A Git fact without an open tree, as after a restart, is a lookup miss, and the provider opens the repository again.

Fed facts come from feeds alone. The projection store keeps the state of each feed object: whether its first answer ended, the sync in which it is due, the one after the sync of its last answer, the count of its kept parts, a version that counts each change of its token and of its kept parts, the digest of its token, and whether it is unlisted; the token itself is apart, and only a call of the feed reads it, so that no size bounds a token and no operation pays for one. An inner state a part brings is a token whose first answer ended, which lists its feed, and which drops the kept parts of its feed unless it is the token that the feed has. An operation on a path whose fact the projection lacks or is fed asks the provider, and a `feed(claims)` answer is served from the projection, or as not found, once the answering feed has answered: the innermost own claim whose feed is unlisted, whose last part said that no inventory of a feed above it has its object, with no state brought by a part of a feed above it since; else the outer claim whose first answer ended; else the innermost one. The facts of a fed object live under the anchor path of its claim, which has the facets of its route at their first choice, for every facet. A part names each anchor of another facet of an object that it writes with the anchor of the claim of the object; the projection keeps the pair, and the host serves the paths under that anchor and its listing as the same paths under the anchor of the claim, without the provider, while the lookup of the anchor stays at its own path, in the listing of its parent. Before the pair, an operation below an anchor of another facet has its fact at the same path under the anchor of its claim. A lookup of a path that the projection lacks, under a fed listing below the anchor of the answering claim that is not complete in the projection, takes the answer of that listing first, with the claims of the listing, and is then served from the projection, since an operation reaches each path by a lookup of each segment from the root, one by a filehandle that it held too; a lookup under a complete listing, or under one at or above that anchor, keeps the rules that follow. A feed with kept parts of a later answer continues that answer from the token of its last kept part for an operation on one of its paths, with the `start` that the rules that follow give the operation, and without one for an operation that they give no answer, in a sync in which the feed is due. A read or a listing that starts that feed without its file or its complete listing in the projection, and a lookup that starts it without its positive lookup before the end of the first answer of the feed, or below the anchor of an own feed after that end too, take an answer with `lacks` and its path under the anchor of the claim as `start`; after that end, another lookup is served from the projection, as not found for a path that the projection lacks, and any other operation takes an answer in a sync in which the feed is due, with `due` and its path under the anchor of the claim as `start` for an operation on one of its starting paths. A check that fails for now, at a rate limit, the network, a timeout, or the upstream, fails no operation that the projection serves: an operation that lacks no fact, on a feed with a state, other than the listing of a first answer that has not ended, is served what the projection has. The operation that lacks its fact, and that listing, have the error without a request. A rate limit with the `Retry-After` or the reset time that its upstream states opens the window of the mount to that instant, which the host compares with the clock, and the feed is asked once more in the same sync after it; a rate limit without one opens no window. Any other failure makes the feed due in the next sync, and holds it back, with the error for an operation that lacks its fact, up to the begin of that sync, so that no retry of an operation is a request of its own. A wait that only a timer could end is none of these: `docs/contracts/10-system.md` names each one that a callout has. The answering feed decides again in its flight, so an operation whose feed another answer moved meanwhile takes that answer. An answer is no publication, and its operation decides again, when the state of its feed has another version than its call began from. The projection counts its publications that changed a fact, and keeps for each path the count of its last change with the count when the call that fetched the change began, and each removal until no call that began before it is in flight or kept. A record or a removal of a call of a feed, a kept part too, writes over no change that the answer of another feed made after the call began: that change stands, the rest of the answer publishes, and after a change of a call that overlaps the call the path counts as lacking, so that the next operation on it asks its feed; bytes that stand are no change to write over. An own feed (`own` in its claims) answers the facts of its own starting paths for an upstream without an inventory of them: once the answering claim needs no answer, or after its answer that the operation ran, the operation asks the innermost own claim inner to the answering one that starts it without its fact, with `lacks` and its path under the anchor of that claim as `start`, the lookup of its anchor only when the feed has no state or is due in a sync, within the window of the mount, and after a failure of the feed, with the error, deciding again in the flight of that feed, and is then served from the projection. One answer runs in a flight per feed object and calls the export until a part without `more`. Each part of the first answer of a feed publishes with its effects, its token, and its inner states in one transition, and a failure keeps the last published token and is the error of each operation that waits for the answer. A restart publishes its token in place of the token of the feed, with the end of its first answer and the sync in which it is due as they were, drops the kept parts of the answer, and its error is the error of each operation that waits for the answer. A part of a later answer, after the end of the first one, is kept with its effects and its inner states, each blob of its effects as the body that it names, durable in the projection store apart from its facts, with the token after the last kept part alone, and the part without `more` publishes the kept parts with it in one transition and drops them; a write of a later part keeps a path that an earlier part removes. A failure keeps the kept parts. The facts of a part have no time, answer filesystems with the zero lifetime of dynamic entries, and are left to their feeds by every other terminal. A part may say that its facts are unsettled, as of an upstream that may have answered before the end of their last change: its facts under the anchor of its object count as lacking from the next sync for the start of an answer, a file or a complete listing, while the projection serves them; the fed mark of each such path keeps that sync, the host refuses an unsettled part of a later answer with `more`, takes it for the facts of the last part of a later answer, and clears it, in that sync or a later one, for the fact that an operation lacked when the answer that the operation started ends without a write of it. A `complete` listing write keeps its listing complete through the lookup reconciliation of its transition, a `join` listing write keeps it as complete as it was, new names too, a removal leaves its parent listing as complete as it was, and a part records as changed the listings, the files, and the removed paths whose facts its publication changed, and none that it wrote as they were: a changed listing reaches filesystems as its directory's attributes changed in place, so the directory and the nodes below it keep their identity, a changed file as its attributes changed in place too, and a removed path is invalidated. The change of the attributes of a path in each answer is of the projection, so that it stands over a restart: the count of the last change of its facts alone, which moves with a change of its content and never back, so that a filesystem lists a directory again only after a sync changed it. In a sync, the getattr of a fed directory asks the feed of its listing once, as its listing would, so that a filesystem that keeps a listing by the attributes of its directory sees what the sync changed; outside a sync, no attribute asks the provider. `projection.names` and `projection.count`, of a listing, and `projection.file`, a range of the bytes of a fed file, each with whether its fact is unsettled, answer only within a feed operation and for a path in the subtree of its object, from the projection alone, without the kept parts. A view leaf of the canonical bytes that a feed part stores for an object is a fed fact with the version of those bytes. The format of the facts of the engine is a part of the identity of a projection, so that a daemon of another format begins an empty projection.

### Listing and lookup

Lookup, listing, and read must use one shared target-resolution model. Listing must be honest about what is currently knowable without inventing provider-specific filesystem behavior.

At each mount root, `.gitignore`, `.ignore`, and `.rgignore` are host-owned
synthetic regular files. Root lookup, listing, and read must agree on that
file kind and fixed content even when a provider or cached dirent projects a
colliding entry; below the mount root, providers may project those names
normally.

An offline partial listing is an honest snapshot of known children, not proof
that unknown children are absent. It must terminate locally without pagination
controls or provider continuations; lookup of a cached child succeeds, an
explicit cached negative remains `NotFound`, and an uncached child remains
`OfflineMiss`.

Keep ordered route precedence in one dispatcher. Durable definitive negative lookups are exact projection facts and must remain coherent with parent listings and object invalidation. Verify parent-directory traversal, not only intended leaf reads.

A listing, or the siblings of a lookup, names a directory child without its own lookup, so it gives the child no lookup fact, and an online lookup of a listed directory asks the provider: only that answer tells a provider directory from the handoff of a Git tree, whose children the host resolves in the tree.

### Live growth

Follow-mode reads, growing sizes, EOF discovery, and invalidation for live files need one shared owner. Filesystem pumps may deliver protocol mechanics, but not semantic rules for file growth, EOF learning, or cached attrs.

## Must not

- Let FUSE and NFS rediscover provider policy independently.
- Let filesystems build projection cache keys or match on cache payload schema.
- Add per-filesystem negative lookup policy, dotfile exceptions, or lookup suppression lists.
- Add parallel provider-facing and wire-facing file structs that can disagree.
- Reintroduce placeholder sizes for unknown-length files.
- Let a filesystem decide whether a learned size is authoritative.
- Add provider-local caches for canonical object bytes.
- Duplicate dispatch ordering in list and lookup paths.
- Let static route scaffolding bind as dynamic captures.

## Code

- `crates/omnifs-engine/src/tree`
- `crates/omnifs-engine/src/runtime`
- `crates/omnifs-engine/src/cache`
- `crates/omnifs-sdk/src/router`
- `crates/omnifs-fuse/src`
- `crates/omnifs-nfs/src`

## Validation

- Add cross-filesystem or tree conformance tests for behavior shared by FUSE and NFS.
- Cache changes need cold and warm read tests, plus invalidation coverage when behavior changes.
- Route/lookup/listing changes need tests that hit lookup, list, and read for the same route surface, including cold and warm paths.
- Size-sensitive changes need stat/read checks and relevant real-tool behavior.
