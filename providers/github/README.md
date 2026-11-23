# omnifs-provider-github

[omnifs](https://github.com/0xff-ai/omnifs) provider that projects GitHub repositories, issues, and pull requests into a filesystem tree. Source trees are bind-mounted clones (cloned on demand via SSH); issues and PRs are per-item directories with title, body, state, and comments as separate files.

## Mount layout

```
/github/{owner}/
  owner.json
  profile.md
/github/{owner}/{repo}/
  repo.json
  issues/
    open|all/
      {number}/
        item.json
        item.md
        title
        body
        state
        user
        comments/{id}/
          comment.json
          comment.md
          author
          body.md
  pulls/
    open|all/
      {number}/
        item.json
        item.md
        title
        body
        state
        user
        comments/{id}/
          comment.json
          comment.md
          author
          body.md
        reviews/{id}/
/github/notifications/thread-{id}/
```

The feed of a repository keeps its issues, pull requests, and comments. The issues and the pull requests come from GitHub GraphQL, each kind in the order of `updatedAt` after the cursor of the last node taken, so that each changed item comes once, with no time and no boundary row; `item.json` is that node and `item.md` its rendering, served without a request of their own. The comments come from the REST list of the comments of the repository.

The first read of `issues/all/`, `issues/open/`, `pulls/all/`, or `pulls/open/` takes the first full listing a page at a time: every issue, every pull request, every comment, the GitHub GraphQL counts of the comments of each item, with the IDs of an item whose comments are other than its count, and the REST count of the comments; a stopped listing continues from its next page at the next read of those folders.

After it, the first read of those folders in each sync is a check, a part of the answer for each request, which the host publishes together with the last part; outside a sync they are served from the projection:

- one GitHub GraphQL `POST` with the issues and the pull requests after their cursors and the count of each kind;
- the comments updated since the second after the last one, the first page with `If-None-Match`: GitHub gives each time to the second, and the last pass took each comment updated before the second of its first answer, so that second after holds no comment and takes each edit and no comment held, whether `since` includes its second or not; when the last comment came in the second of that answer or the one before, the pass asks from the second that may hold more; a comment that comes twice in one pass moved past a page while the pass paged, and the pass asks again;
- the REST count of the comments of the repository, by the page of one comment at the last position of the count held, with its `ETag`: `304` keeps the count, since each other count has another comment at that position or none;
- the IDs of each changed item whose comments are other than its count, which remove a deleted comment and bring by its ID a comment that the list lacks;
- for a count of a kind other than the projection's with the changes of the check, the numbers of that kind, once, which remove the items that left and bring those that the projection lacks;
- for a REST count of the comments other than the count the projection holds with the changes of the check, the counts of the comments of the items until each item whose comments changed unseen, as by a deletion, took its IDs.

A quiet check is one `POST` and two `304`s.

Before the first full listing ends, an item answers from GitHub GraphQL by its number, and its comments download at the first read of its `comments/` or of a comment in it, for a count other than `0`. An item that GitHub answers for its number from another repository or with another number, as an issue that moved, and an item whose comments answer `301`, left the repository. A repository that GitHub GraphQL names otherwise, or whose REST requests answer `301`, is not at its path. The facts of an issue or a pull request live under `all/`, which `open/` shares while it is open. GitHub GraphQL answers only an authenticated mount.

The owner and the repository are named as the path spells them. The sync program asks GitHub for their canonical spelling and reads that path alone; a path in another spelling is a repository of its own, which takes its own first full listing and shows its own whole tree from it, never one mixed with the facts of another spelling, and is never part of a sync.

The entries of `reviews/` and `reviews/{id}/comments/` come with the node of their pull request, without the code beside an inline comment, and a page is the last when its answer has no link `next`. `diff.patch` is the diff between the base and the head of the pull request, which its node in the projection names, without a request for them; the host downloads the diff of a pair of commits once. A read of the diff of a pull request that the projection lacks is not found. A secondary rate limit without `Retry-After` states no window, and the feed is asked again in the next sync.

## Capabilities

`api.github.com` over HTTPS. The default user workflow is `omnifs mount add github`, which runs GitHub's device flow with product client id `Ov23licogxMDzS47s9sF` and no default scopes. That default is public-read only; use `omnifs mount add github --scope repo` only when you need private repository access and accept GitHub OAuth's broad private-repository grant. Use `omnifs mount reauth github` to repair a missing or expired credential after the mount exists, or `omnifs mount add github --token-env GITHUB_TOKEN` to authenticate with a personal access token from an environment variable. Read-only today; mutation path WIP per the design docs.

## Install

This is a wasm component. Build with:

```bash
cargo build --target wasm32-wasip2 --release -p omnifs-provider-github
```

Release CLI binaries embed this provider and unpack it into `OMNIFS_HOME/providers`. Provider metadata and the auth section are authored from `#[omnifs_sdk::provider]` annotations and embedded in the wasm `omnifs.provider-metadata.v1` section at build time; `omnifs mount add github` reads that metadata from the wasm.

## Status

Pre-1.0. Mount layout and projection rules may evolve.

## License

Dual licensed under MIT or Apache-2.0 at your option.
