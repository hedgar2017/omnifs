#![cfg(not(target_os = "wasi"))]

mod feeds;
mod repo;
mod support;

use omnifs_engine::Namespace;
use omnifs_itest::ReadFileOpExt;
use omnifs_itest::support::answering;
use omnifs_wit::host::types::{
    CalloutResult, Cursor, EntryKind, Header, HttpResponse, ListChildrenResult, LookupChildResult,
    PageValidator, ReadFileOutcome, Stability,
};
use support::{
    TestOpExt, fetched, github_harness, http, not_modified, project_file_stability, project_paths,
    request_header, resolve_namespace,
};

#[test]
fn github_root_readme_snapshot() {
    let harness = github_harness();
    let readme = harness
        .read("/README.md")
        .unwrap()
        .into_read_file()
        .unwrap();
    let actual = String::from_utf8(omnifs_itest::expect_inline(&readme).to_vec()).unwrap();
    let expected = include_str!("snapshots/root-readme.md").trim_end();
    if actual.trim_end() != expected {
        eprintln!("{actual}");
    }
    assert_eq!(actual.trim_end(), expected);
}

#[test]
fn github_provider_routes_namespace_and_numeric_paths() {
    let harness = github_harness();
    // listing a repo dir triggers repo_gate, which fetches /repos/{owner}/{repo}
    // to gate existence. Stub a 200 to confirm the repo exists, then the router
    // merges the literal children (repo/issues/pulls/actions).
    let mut repo_listing = harness.list("/octocat/Hello-World").unwrap();
    let gate_fetch = repo_listing.expect_single_fetch();
    assert!(
        gate_fetch.url.ends_with("/repos/octocat/Hello-World"),
        "unexpected repo gate URL: {}",
        gate_fetch.url
    );
    repo_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"name":"Hello-World"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    match repo_listing.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec!["actions", "issues", "pulls", "repo", "repo.json"]
            );
        },
        other => panic!("expected repo namespace listing, got {other:?}"),
    }
    // lookup of `runs` under `actions` is structural (no fetch). The path
    // `actions/runs` is an implicit prefix dir because dir routes extend under it.
    let runs_lookup = harness
        .lookup("/octocat/Hello-World/actions", "runs")
        .unwrap();
    match runs_lookup.result().unwrap() {
        Ok(LookupChildResult::Entry(result)) => {
            assert_eq!(result.target.name, "runs");
            assert!(
                matches!(result.target.kind, EntryKind::Directory),
                "runs should resolve as directory"
            );
        },
        other => panic!("expected structural runs dir lookup, got {other:?}"),
    }

    // Listing `runs` DOES fetch from GitHub.
    let mut runs_listed = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    let runs_fetch = runs_listed.expect_single_fetch();
    assert!(
        runs_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30&page=1"),
        "unexpected action runs listing URL: {}",
        runs_fetch.url
    );

    runs_listed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"{
                "workflow_runs": [
                    {"id":123,"status":"completed","conclusion":"success"}
                ]
            }"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match runs_listed.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["123"]);
        },
        other => panic!("expected runs listing, got {other:?}"),
    }
}

#[test]
fn github_action_run_list_projects_files() {
    use omnifs_wit::host::types::{Callout, CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut response = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    assert!(
        response.is_waiting_for_callouts(),
        "expected callout wait, got {response:?}"
    );
    let [Callout::Fetch(fetch)] = response.callouts() else {
        panic!("expected fetch callout, got {:?}", response.callouts());
    };
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30&page=1"),
        "unexpected action runs URL: {}",
        fetch.url
    );

    response
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"{
                "workflow_runs":[
                    {
                        "id":123,
                        "status":"completed",
                        "conclusion":"success"
                    }
                ]
            }"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();

    assert!(
        response.callouts().is_empty(),
        "list terminal should carry no callouts, got {:?}",
        response.callouts()
    );
    match response.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            // Each row of the list is the canonical bytes of its run, from
            // which its files render without a request.
            let effects = response.effects().unwrap();
            assert_eq!(canonical_bytes(effects).len(), 1);
            assert!(
                std::str::from_utf8(canonical_bytes(effects)[0])
                    .unwrap()
                    .contains(r#""id":123"#)
            );
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["123"]);
        },
        other => panic!("expected action run listing terminal, got {other:?}"),
    }
}

#[test]
fn github_provider_action_run_lookup_validates_and_listing_validates() {
    use omnifs_wit::host::types::{CalloutResult, Header, HttpResponse};

    let harness = github_harness();
    // lookup_child on a run id is structural (no fetch). The run id path is
    // an implicit literal-prefix dir (because `/{run_id}/log` is a registered
    // file route under it), so it resolves immediately as a directory.
    let lookup = harness
        .lookup("/octocat/Hello-World/actions/runs", "123")
        .unwrap();
    match lookup.result().unwrap() {
        Ok(LookupChildResult::Entry(result)) => {
            let entry = &result.target;
            assert_eq!(entry.name, "123");
            assert!(matches!(entry.kind, EntryKind::Directory));
        },
        other => panic!("expected structural run dir lookup, got {other:?}"),
    }

    // Listing the run dir DOES fetch the run to resolve status/conclusion.
    let mut issued = harness
        .list("/octocat/Hello-World/actions/runs/123")
        .unwrap();
    assert!(
        issued.is_waiting_for_callouts(),
        "expected action run listing to dispatch validation, got {issued:?}"
    );

    issued
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"id":123,"status":"completed","conclusion":"success"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();

    match issued.result().unwrap() {
        Ok(ListChildrenResult::Entries(result)) => {
            let names: Vec<&str> = result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(names.contains(&"status"), "missing status in {names:?}");
            assert!(
                names.contains(&"conclusion"),
                "missing conclusion in {names:?}"
            );
            assert!(names.contains(&"logs.zip"), "missing logs.zip in {names:?}");
            assert!(
                !names.contains(&"comments"),
                "unexpected item comments route leaked into action run listing: {names:?}"
            );
            // status and conclusion are preloaded via preload_file on the dir listing.
            let preloaded = project_paths(issued.effects().unwrap());
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/status"),
                "missing status preload in {preloaded:?}"
            );
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/conclusion"),
                "missing conclusion preload in {preloaded:?}"
            );
        },
        other => panic!("expected list entries(123) after 200, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_owner_listing_tracks_browsed_repos() {
    // The owner is an OBJECT at `/{owner}`: listing it MERGES the owner's own
    // faces (owner.json canonical, profile.md representation) with the repo
    // names yielded by the `Owner::repos` anchor collection. The repo names ARE
    // the child `Repo` anchors (`/{owner}/{repo}`). This test pins that merge:
    // a browsed repo surfaces under the owner alongside the owner's own faces.
    let harness = github_harness();
    // listing /{owner}/{repo} triggers repo_gate which fetches the repo API.
    let mut repo_listing = harness.list("/octocat/Hello-World").unwrap();
    let gate_fetch = repo_listing.expect_single_fetch();
    assert!(
        gate_fetch.url.ends_with("/repos/octocat/Hello-World"),
        "expected repo gate URL, got {}",
        gate_fetch.url
    );
    repo_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"name":"Hello-World"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    assert!(
        repo_listing.result().unwrap().is_ok(),
        "expected repo listing after gate, got {repo_listing:?}"
    );

    // Listing the owner anchor fetches (1) the owner profile to load its own
    // canonical (owner.json/profile.md faces), (2) the repos of an
    // organization, which answer 404 for a user, then (3) the repos of the
    // user.
    let mut owner_listing = harness.list("/octocat").unwrap();
    let owner_load = owner_listing.expect_single_fetch();
    assert!(
        owner_load.url.ends_with("/users/octocat"),
        "expected owner profile load first, got {}",
        owner_load.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"octocat","type":"User"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    let org_repos_fetch = owner_listing.expect_single_fetch();
    assert!(
        org_repos_fetch
            .url
            .ends_with("/orgs/octocat/repos?sort=full_name&direction=asc&per_page=100&page=1"),
        "expected the repos of an organization first, got {}",
        org_repos_fetch.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::<Header>::new(),
            body: br#"{"message":"Not Found"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    let repos_fetch = owner_listing.expect_single_fetch();
    assert!(
        repos_fetch
            .url
            .ends_with("/users/octocat/repos?sort=full_name&direction=asc&per_page=100&page=1"),
        "expected owner repo listing fetch, got {}",
        repos_fetch.url
    );

    let repo_page = (0..100)
        .map(|index| {
            if index == 0 {
                r#"{"name":"Hello-World"}"#.to_string()
            } else {
                format!(r#"{{"name":"repo-{index:03}"}}"#)
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "ETag".to_string(),
                value: r#""repos-1""#.to_string(),
            }],
            body: format!("[{repo_page}]").into_bytes(),
            redirected: None,
        })])
        .unwrap();
    // The validator and the cursor of a page carry the owner kind, so a page
    // asked for again needs no classification.
    let validator = r#"{"kind":"user","etag":"\"repos-1\""}"#;
    match owner_listing.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            // The merge: owner faces AND the browsed repo name share one listing.
            assert!(
                names.contains(&"Hello-World"),
                "expected browsed repo in owner listing, got {names:?}"
            );
            assert!(
                names.contains(&"owner.json"),
                "expected owner.json face in owner listing, got {names:?}"
            );
            assert!(
                names.contains(&"profile.md"),
                "expected profile.md face in owner listing, got {names:?}"
            );
            assert!(matches!(
                listing.next_cursor,
                Some(Cursor::Opaque(ref cursor)) if cursor == r#"{"kind":"user","page":2}"#
            ));
            let page_validator = listing
                .validator
                .as_ref()
                .expect("the page has a validator");
            assert_eq!(page_validator.token, validator);
            assert_eq!(page_validator.interval, None);
        },
        other => panic!("expected owner listing, got {other:?}"),
    }

    let mut first_again = harness
        .list_page("/octocat", None, Some(validator))
        .unwrap();
    // The unchanged page answers without a load of the owner object.
    let repos_again = first_again.expect_single_fetch();
    assert!(
        repos_again
            .url
            .ends_with("/users/octocat/repos?sort=full_name&direction=asc&per_page=100&page=1"),
        "a page asked for again goes straight to the repos of its owner kind: {}",
        repos_again.url
    );
    assert!(repos_again.headers.iter().any(|header| {
        header.name.eq_ignore_ascii_case("if-none-match") && header.value == r#""repos-1""#
    }));
    first_again
        .answer_callouts(vec![CalloutResult::HttpResponse(not_modified())])
        .unwrap();
    assert!(matches!(
        first_again.result().unwrap(),
        Ok(ListChildrenResult::Unchanged(Some(PageValidator { token, interval: None })))
            if token == validator
    ));

    // A changed first page has the faces of the owner without a load of the
    // owner.
    let mut first_changed = harness
        .list_page("/octocat", None, Some(validator))
        .unwrap();
    let repos_changed = first_changed.expect_single_fetch();
    assert!(
        repos_changed
            .url
            .ends_with("/users/octocat/repos?sort=full_name&direction=asc&per_page=100&page=1"),
        "a changed page goes straight to the repos of its owner kind: {}",
        repos_changed.url
    );
    first_changed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "ETag".to_string(),
                value: r#""repos-2""#.to_string(),
            }],
            body: br#"[{"name":"Hello-World"},{"name":"new-repo"}]"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    match first_changed.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            for name in ["owner.json", "profile.md", "Hello-World", "new-repo"] {
                assert!(names.contains(&name), "{name} in {names:?}");
            }
        },
        other => panic!("expected the changed owner page, got {other:?}"),
    }

    // A later page goes straight to the repos of its owner kind, without a
    // load of the owner, and has none of its faces.
    let mut page_two = harness
        .list_page(
            "/octocat",
            Some(&Cursor::Opaque(r#"{"kind":"user","page":2}"#.to_string())),
            None,
        )
        .unwrap();
    let repos_fetch = page_two.expect_single_fetch();
    assert!(
        repos_fetch
            .url
            .ends_with("/users/octocat/repos?sort=full_name&direction=asc&per_page=100&page=2"),
        "expected stable owner repo page 2 URL, got {}",
        repos_fetch.url
    );
    page_two
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "ETag".to_string(),
                value: r#""repos-page-2""#.to_string(),
            }],
            body: br#"[{"name":"z-last"}]"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    match page_two.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, ["z-last"]);
        },
        other => panic!("expected the second owner page, got {other:?}"),
    }
}

#[test]
fn github_root_and_owner_listings_ignore_unclassified_repo_paths() {
    // listing /{owner}/{repo} triggers repo_gate which fetches /repos/{owner}/{repo}.
    // Complete the fetch for each repo path before verifying owner/root listings.
    let harness = github_harness();

    for path in ["/zeta/zulu", "/open/source", "/alpha/app", "/openai/api"] {
        let mut step = harness.list(path).unwrap();
        assert!(
            step.is_waiting_for_callouts(),
            "expected repo gate fetch for {path}, got {step:?}"
        );
        step.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: b"{}".to_vec(),
            redirected: None,
        })])
        .unwrap();
        assert!(
            step.result().unwrap().is_ok(),
            "expected repo listing for {path}, got {step:?}"
        );
    }

    let root_listing = harness.list("/").unwrap();
    match root_listing.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["README.md", "notifications"]);
        },
        other => panic!("expected root listing, got {other:?}"),
    }

    let mut owner_listing = harness.list("/open").unwrap();
    let owner_load = owner_listing.expect_single_fetch();
    assert!(
        owner_load.url.ends_with("/users/open"),
        "expected owner profile load first, got {}",
        owner_load.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::<Header>::new(),
            body: br#"{"login":"open","type":"User"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    let org_repos_fetch = owner_listing.expect_single_fetch();
    assert!(
        org_repos_fetch
            .url
            .ends_with("/orgs/open/repos?sort=full_name&direction=asc&per_page=100&page=1"),
        "expected the repos of an organization first, got {}",
        org_repos_fetch.url
    );
    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::<Header>::new(),
            body: br#"{"message":"Not Found"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    let repos_fetch = owner_listing.expect_single_fetch();
    assert!(
        repos_fetch
            .url
            .ends_with("/users/open/repos?sort=full_name&direction=asc&per_page=100&page=1"),
        "expected owner repo listing fetch, got {}",
        repos_fetch.url
    );

    owner_listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: b"[]".to_vec(),
            redirected: None,
        })])
        .unwrap();
    match owner_listing.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            // The owner listing is its own faces merged with the API-reported
            // repos (here none). The earlier browsed `/open/source` path must
            // NOT surface as a phantom repo child: unclassified scaffolding does
            // not bind. Only the object's own faces remain.
            let mut repo_children: Vec<&str> = names
                .iter()
                .copied()
                .filter(|name| *name != "owner.json" && *name != "profile.md")
                .collect();
            repo_children.sort_unstable();
            assert!(
                repo_children.is_empty(),
                "unexpected repo children after uncached repo traversal: {repo_children:?}"
            );
        },
        other => panic!("expected owner listing, got {other:?}"),
    }
}

#[test]
fn github_provider_missing_item_resources_validate_on_lookup() {
    use omnifs_wit::host::types::{CalloutResult, Header, HttpResponse};

    // An issue anchor answers the claims of the feeds of the repository and
    // of the issue without a request: its existence comes from the feeds.
    // The claim of the issue has its anchor in `all/`, the first facet, where
    // its facts live for `open/` too.
    let harness = github_harness();

    let lookup = harness
        .lookup("/octocat/Hello-World/issues/open", "999999999")
        .unwrap();
    assert!(lookup.callouts().is_empty());
    match lookup.result().unwrap() {
        Ok(LookupChildResult::Feed(claims)) => {
            let claims = claims
                .iter()
                .map(|claim| (claim.object.path.as_str(), claim.starts))
                .collect::<Vec<_>>();
            assert_eq!(
                claims,
                [
                    ("/octocat/Hello-World", false),
                    ("/octocat/Hello-World/issues/all/999999999", true),
                ]
            );
        },
        other => panic!("expected the claims of the issue anchor, got {other:?}"),
    }

    let diff_lookup = harness
        .lookup("/octocat/Hello-World/issues/open/999999999", "diff.patch")
        .unwrap();
    match diff_lookup.result().unwrap() {
        Ok(LookupChildResult::NotFound(_)) => {},
        other => panic!("expected issue diff.patch lookup to be NotFound, got {other:?}"),
    }

    // Reading a face under a missing comment object triggers a fetch; a 404
    // from GitHub propagates as NotFound. A comment is a `{comment_id}` object
    // dir, so the fetchable leaf is `comments/1/comment.json`, which loads the
    // standalone comment GET keyed by comment id.
    let mut issued = harness
        .read("/octocat/Hello-World/issues/open/999999999/comments/1/comment.json")
        .unwrap();
    let fetch = issued.expect_single_fetch();
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/1"),
        "unexpected comment fetch URL: {}",
        fetch.url
    );

    issued
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::<Header>::new(),
            body: b"{\"message\":\"Not Found\"}".to_vec(),
            redirected: None,
        })])
        .unwrap();

    // A 404 on the comment object load resolves the object as not-found, keyed
    // to the comment anchor.
    match issued.result().unwrap() {
        Ok(ReadFileOutcome::NotFound(_)) => {},
        other => panic!("expected ReadFile NotFound on 404 read, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_pr_lookup_is_fed_and_exposes_diff() {
    use omnifs_wit::host::types::{BlobContent, BlobFetched, ByteSource, Callout, CalloutResult};

    let harness = github_harness();

    let lookup = harness
        .lookup("/octocat/Hello-World/pulls/open", "7")
        .unwrap();
    assert!(lookup.callouts().is_empty());
    match lookup.result().unwrap() {
        Ok(LookupChildResult::Feed(claims)) => {
            assert_eq!(claims.len(), 2);
            assert_eq!(claims[1].object.id.kind, "github.pull");
            assert_eq!(claims[1].object.path, "/octocat/Hello-World/pulls/all/7");
        },
        other => panic!("expected the claims of the PR anchor, got {other:?}"),
    }

    // The fed anchor lists its faces without a load of the pull request.
    let listing = harness.list("/octocat/Hello-World/pulls/open/7").unwrap();
    assert!(listing.callouts().is_empty());
    match listing.result().unwrap() {
        Ok(ListChildrenResult::Entries(result)) => {
            let mut names: Vec<&str> = result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec![
                    "body",
                    "checks",
                    "comments",
                    "diff.patch",
                    "files",
                    "item.json",
                    "item.md",
                    "reviews",
                    "state",
                    "title",
                    "user",
                ]
            );
        },
        other => panic!("expected PR dir listing, got {other:?}"),
    }

    // The diff is the diff between the base and the head that the pull
    // request names: the host holds the diff of each pair of commits, so a
    // read asks GitHub for the diff of a pair once, and a push to the pull
    // request, which moves its head, asks for another diff.
    for (head, blob) in [("head-1", 1), ("head-2", 2)] {
        let mut diff = harness
            .read("/octocat/Hello-World/pulls/open/7/diff.patch")
            .unwrap();
        let pull_fetch = diff.expect_single_fetch();
        assert!(
            pull_fetch
                .url
                .ends_with("/repos/octocat/Hello-World/pulls/7"),
            "unexpected pull request URL: {}",
            pull_fetch.url
        );
        diff.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: format!(r#"{{"base":{{"sha":"base-1"}},"head":{{"sha":"{head}"}}}}"#)
                .into_bytes(),
            redirected: None,
        })])
        .unwrap();
        let diff_fetch = match diff.callouts() {
            [Callout::FetchBlob(request)] => request,
            other => panic!("expected fetch-blob callout, got {other:?}"),
        };
        assert!(
            diff_fetch
                .url
                .ends_with("/repos/octocat/Hello-World/pulls/7"),
            "unexpected PR diff fetch URL: {}",
            diff_fetch.url
        );
        assert!(
            diff_fetch
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("accept")
                    && h.value == "application/vnd.github.diff"),
            "expected diff Accept header, got {:?}",
            diff_fetch.headers
        );
        let version = format!("base-1..{head}");
        assert_eq!(diff_fetch.version.as_deref(), Some(version.as_str()));

        diff.answer_callouts(vec![CalloutResult::BlobFetched(BlobFetched {
            status: 200,
            response_headers: Vec::new(),
            redirected: None,
            upstream: true,
            content: Some(BlobContent { blob, size: 25 }),
        })])
        .unwrap();

        match diff.result().unwrap() {
            Ok(ReadFileOutcome::Found(file)) => {
                match &file.bytes {
                    ByteSource::Blob(found) => assert_eq!(*found, blob),
                    other => panic!("expected blob-backed diff, got {other:?}"),
                }
                assert_eq!(file.attrs.stability, Stability::Dynamic);
                assert_eq!(file.attrs.version_token.as_deref(), Some(version.as_str()));
            },
            other => panic!("expected PR diff file after read, got {other:?}"),
        }
    }
}

#[test]
fn github_pr_files_list_and_read_changed_file_objects() {
    use omnifs_wit::host::types::{CalloutResult, HttpResponse};

    const CHANGED_FILE_ROW: &[u8] = br#"{"changes":7,"unknown_marker":"preserved","filename":"src/lib.rs","deletions":2,"status":"modified","patch":"@@ -1 +1 @@\n-old\n+new","additions":5}"#;
    let files_response = [b"[".as_slice(), CHANGED_FILE_ROW, b"]".as_slice()].concat();
    let harness = github_harness();
    let mut listed = harness
        .list("/octocat/Hello-World/pulls/open/7/files")
        .unwrap();
    let fetch = listed.expect_single_fetch();
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/files?per_page=100&page=1"),
        "unexpected PR files URL: {}",
        fetch.url
    );

    listed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: files_response.clone(),
            redirected: None,
        })])
        .unwrap();
    match listed.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["src%2Flib.rs"]);
            let effects = listed.effects().unwrap();
            assert_eq!(
                canonical_bytes(effects),
                [CHANGED_FILE_ROW],
                "the row of a changed file is its canonical bytes"
            );
        },
        other => panic!("expected PR files listing, got {other:?}"),
    }

    let mut read = harness
        .read("/octocat/Hello-World/pulls/open/7/files/src%2Flib.rs/file.md")
        .unwrap();
    let read_fetch = read.expect_single_fetch();
    assert!(
        read_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/files?per_page=100&page=1"),
        "unexpected changed-file read URL: {}",
        read_fetch.url
    );
    read.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: files_response,
        redirected: None,
    })])
    .unwrap();
    match read.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(body.contains("# src/lib.rs"), "unexpected file.md: {body}");
            assert_eq!(
                read.effects().unwrap().canonical[0].bytes.as_slice(),
                CHANGED_FILE_ROW,
                "changed-file canonical must preserve the matched list row verbatim"
            );
        },
        other => panic!("expected changed-file markdown read, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_pr_reviews_and_review_comments_list_and_read_objects() {
    use omnifs_wit::host::types::{CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut reviews = harness
        .list("/octocat/Hello-World/pulls/open/7/reviews")
        .unwrap();
    let fetch = reviews.expect_single_fetch();
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/reviews?per_page=100&page=1"),
        "unexpected reviews URL: {}",
        fetch.url
    );
    reviews
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"[
                {
                    "id":80,
                    "state":"APPROVED",
                    "body":"Looks good",
                    "user":{"login":"reviewer"}
                }
            ]"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match reviews.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["80"]);
            let effects = reviews.effects().unwrap();
            assert_eq!(
                canonical_bytes(effects).len(),
                1,
                "the row of a review is its canonical bytes"
            );
        },
        other => panic!("expected reviews listing, got {other:?}"),
    }

    let mut review_md = harness
        .read("/octocat/Hello-World/pulls/open/7/reviews/80/review.md")
        .unwrap();
    let review_fetch = review_md.expect_single_fetch();
    assert!(
        review_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7/reviews/80"),
        "unexpected review read URL: {}",
        review_fetch.url
    );
    review_md
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id":80,
                "state":"APPROVED",
                "body":"Looks good",
                "user":{"login":"reviewer"}
            }"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match review_md.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(body.contains("APPROVED"), "unexpected review.md: {body}");
        },
        other => panic!("expected review markdown read, got {other:?}"),
    }

    let mut comments = harness
        .list("/octocat/Hello-World/pulls/open/7/reviews/80/comments")
        .unwrap();
    let comments_fetch = comments.expect_single_fetch();
    assert!(
        comments_fetch.url.ends_with(
            "/repos/octocat/Hello-World/pulls/7/reviews/80/comments?per_page=100&page=1"
        ),
        "unexpected review comments URL: {}",
        comments_fetch.url
    );
    comments
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"[
                {
                    "id":99,
                    "body":"nit",
                    "path":"src/lib.rs",
                    "diff_hunk":"@@ -1 +1 @@",
                    "user":{"login":"reviewer"}
                }
            ]"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match comments.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["99"]);
            assert_eq!(
                canonical_bytes(comments.effects().unwrap()).len(),
                1,
                "the row of a review comment is its canonical bytes"
            );
        },
        other => panic!("expected review comments listing, got {other:?}"),
    }

    let mut comment_md = harness
        .read("/octocat/Hello-World/pulls/open/7/reviews/80/comments/99/comment.md")
        .unwrap();
    let comment_md_fetch = comment_md.expect_single_fetch();
    assert!(
        comment_md_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/comments/99"),
        "unexpected review comment read URL: {}",
        comment_md_fetch.url
    );
    comment_md
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id":99,
                "body":"nit",
                "path":"src/lib.rs",
                "diff_hunk":"@@ -1 +1 @@",
                "user":{"login":"reviewer"}
            }"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match comment_md.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(
                body.contains("reviewer on `src/lib.rs`"),
                "unexpected comment.md: {body}"
            );
        },
        other => panic!("expected review comment markdown read, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_pr_checks_list_from_head_sha_and_read_check_run_objects() {
    use omnifs_wit::host::types::{CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut checks = harness
        .list("/octocat/Hello-World/pulls/open/7/checks")
        .unwrap();
    let pull_fetch = checks.expect_single_fetch();
    assert!(
        pull_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/pulls/7"),
        "unexpected PR head URL: {}",
        pull_fetch.url
    );
    checks
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""pull-1""#)],
            body: br#"{"base":{"sha":"base123"},"head":{"sha":"abc123"}}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    let check_fetch = checks.expect_single_fetch();
    assert!(
        check_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/commits/abc123/check-runs?per_page=100&page=1"),
        "unexpected check runs URL: {}",
        check_fetch.url
    );
    let first_page = (700..800)
        .map(|id| {
            let output = if id == 700 {
                r#"{"title":"CI","summary":"All green","text":null}"#
            } else {
                r#"{"title":null,"summary":null,"text":null}"#
            };
            format!(
                r#"{{
                    "id":{id},
                    "name":"ci-{id}",
                    "status":"completed",
                    "conclusion":"success",
                    "output":{output}
                }}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    checks
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""runs-1""#)],
            body: format!(r#"{{"check_runs":[{first_page}]}}"#).into_bytes(),
            redirected: None,
        })])
        .unwrap();
    // The first page's validator carries the ETag and the head SHA of the pull
    // request, so the page asked for again makes conditional requests alone.
    let validator = r#"{"pull":{"etag":"\"pull-1\"","sha":"abc123"},"runs":"\"runs-1\""}"#;
    match checks.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names.len(), 100);
            assert!(names.contains(&"700"));
            assert!(matches!(
                listing.next_cursor,
                Some(Cursor::Opaque(ref cursor))
                    if cursor == r#"{"sha":"abc123","page":2}"#
            ));
            assert_eq!(
                canonical_bytes(checks.effects().unwrap()).len(),
                100,
                "the row of a check run is its canonical bytes"
            );
            assert_eq!(
                listing
                    .validator
                    .as_ref()
                    .map(|validator| validator.token.as_str()),
                Some(validator)
            );
        },
        other => panic!("expected check runs listing, got {other:?}"),
    }

    let mut first_again = harness
        .list_page(
            "/octocat/Hello-World/pulls/open/7/checks",
            None,
            Some(validator),
        )
        .unwrap();
    let pull_again = first_again.expect_single_fetch();
    assert_eq!(
        request_header(pull_again, "if-none-match"),
        Some(r#""pull-1""#)
    );
    first_again
        .answer_callouts(vec![CalloutResult::HttpResponse(not_modified())])
        .unwrap();
    let runs_again = first_again.expect_single_fetch();
    assert!(
        runs_again
            .url
            .ends_with("/repos/octocat/Hello-World/commits/abc123/check-runs?per_page=100&page=1")
    );
    assert_eq!(
        request_header(runs_again, "if-none-match"),
        Some(r#""runs-1""#)
    );
    first_again
        .answer_callouts(vec![CalloutResult::HttpResponse(not_modified())])
        .unwrap();
    assert!(matches!(
        first_again.result().unwrap(),
        Ok(ListChildrenResult::Unchanged(Some(PageValidator { token, interval: None })))
            if token == validator
    ));

    // A changed pull request with the same head asks for its check runs with
    // their `ETag`, and their `304` keeps the page with the new `ETag` of the
    // pull request.
    let mut changed_pull = harness
        .list_page(
            "/octocat/Hello-World/pulls/open/7/checks",
            None,
            Some(validator),
        )
        .unwrap();
    changed_pull
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""pull-2""#)],
            body: br#"{"base":{"sha":"base123"},"head":{"sha":"abc123"}}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    assert_eq!(
        request_header(changed_pull.expect_single_fetch(), "if-none-match"),
        Some(r#""runs-1""#)
    );
    changed_pull
        .answer_callouts(vec![CalloutResult::HttpResponse(not_modified())])
        .unwrap();
    assert!(matches!(
        changed_pull.result().unwrap(),
        Ok(ListChildrenResult::Unchanged(Some(PageValidator { token, interval: None })))
            if token == r#"{"pull":{"etag":"\"pull-2\"","sha":"abc123"},"runs":"\"runs-1\""}"#
    ));

    let cursor = r#"{"sha":"abc123","page":2}"#.to_string();
    let mut page_two = harness
        .list_page(
            "/octocat/Hello-World/pulls/open/7/checks",
            Some(&Cursor::Opaque(cursor)),
            None,
        )
        .unwrap();
    let continuation_fetch = page_two.expect_single_fetch();
    assert!(
        continuation_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/commits/abc123/check-runs?per_page=100&page=2"),
        "continuation must reuse the captured head without refetching the pull: {}",
        continuation_fetch.url
    );
    page_two
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"{"check_runs":[]}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    match page_two.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            assert!(listing.entries.is_empty());
            assert!(listing.exhaustive);
            assert!(listing.next_cursor.is_none());
        },
        other => panic!("expected completed check continuation, got {other:?}"),
    }

    let mut check_md = harness
        .read("/octocat/Hello-World/pulls/open/7/checks/700/check.md")
        .unwrap();
    let read_fetch = check_md.expect_single_fetch();
    assert!(
        read_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/check-runs/700"),
        "unexpected check run read URL: {}",
        read_fetch.url
    );
    check_md
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{
                "id":700,
                "name":"ci",
                "status":"completed",
                "conclusion":"success",
                "html_url":"https://github.com/octocat/Hello-World/runs/700",
                "output":{"title":"CI","summary":"All green"}
            }"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match check_md.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(body.contains("# ci"), "unexpected check.md: {body}");
            assert!(body.contains("All green"), "unexpected check.md: {body}");
        },
        other => panic!("expected check run markdown read, got {other:?}"),
    }
}

#[test]
fn github_notifications_list_and_read_thread_objects() {
    use omnifs_wit::host::types::{CalloutResult, HttpResponse};

    let harness = github_harness();
    let mut listed = harness.list("/notifications").unwrap();
    let fetch = listed.expect_single_fetch();
    assert!(
        fetch.url.ends_with("/notifications?per_page=50&page=1"),
        "unexpected notifications URL: {}",
        fetch.url
    );
    listed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"[
                {
                    "id":"123",
                    "unread":true,
                    "reason":"mention",
                    "updated_at":"2026-07-05T00:00:00Z",
                    "subject":{"title":"Review requested","type":"PullRequest"},
                    "repository":{"full_name":"octocat/Hello-World"}
                }
            ]"#
            .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match listed.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            // The generated branch README (route-schema synthesis) lists
            // alongside the notification threads.
            assert_eq!(names, vec!["README.md", "thread-123"]);
            let effects = listed.effects().unwrap();
            assert!(
                effects.canonical.is_empty(),
                "notification list must not seed thread canonicals, got {:?}",
                effects.canonical
            );
        },
        other => panic!("expected notifications listing, got {other:?}"),
    }

    let mut item = harness.read("/notifications/thread-123/item.md").unwrap();
    let read_fetch = item.expect_single_fetch();
    assert!(
        read_fetch.url.ends_with("/notifications/threads/123"),
        "unexpected notification read URL: {}",
        read_fetch.url
    );
    item.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: br#"{
            "id":"123",
            "unread":true,
            "reason":"mention",
            "updated_at":"2026-07-05T00:00:00Z",
            "subject":{"title":"Review requested","type":"PullRequest"},
            "repository":{"full_name":"octocat/Hello-World"}
        }"#
        .to_vec(),
        redirected: None,
    })])
    .unwrap();
    match item.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            let body = std::str::from_utf8(omnifs_itest::expect_inline(file)).unwrap();
            assert!(
                body.contains("# Review requested"),
                "unexpected notification item.md: {body}"
            );
        },
        other => panic!("expected notification item read, got {other:?}"),
    }
}

#[test]
fn github_projected_resource_reads_return_all_fetched_siblings() {
    // per-field files (title, body, state, user, status, conclusion) are
    // descoped; sibling preloads happen at listing time via run_dir and runs_list.
    //
    // This test verifies that listing a specific run directory fetches the run
    // and preloads its status and conclusion alongside the listing (so the host
    // can cache them without a second round-trip).
    use omnifs_wit::host::types::{CalloutResult, HttpResponse};

    let harness = github_harness();

    // list_children on a run dir fetches the run and preloads status/conclusion.
    let mut run_listed = harness
        .list("/octocat/Hello-World/actions/runs/123")
        .unwrap();
    let run_fetch = run_listed.expect_single_fetch();
    assert!(
        run_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs/123"),
        "unexpected action run dir URL: {}",
        run_fetch.url
    );

    run_listed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"id":123,"status":"completed","conclusion":"success"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    match run_listed.result().unwrap() {
        Ok(ListChildrenResult::Entries(_)) => {
            let effects = run_listed.effects().unwrap();
            let preloaded = project_paths(effects);
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/status"),
                "missing status preload in {preloaded:?}"
            );
            assert!(
                preloaded.contains(&"/octocat/Hello-World/actions/runs/123/conclusion"),
                "missing conclusion preload in {preloaded:?}"
            );
            assert_eq!(
                project_file_stability(effects, "/octocat/Hello-World/actions/runs/123/conclusion"),
                Some(Stability::Dynamic),
                "conclusion should be Dynamic"
            );
        },
        other => panic!("expected run dir listing with preloads, got {other:?}"),
    }

    // list_children on the runs directory also preloads at the full mount-relative path.
    let mut index_listed = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    let index_fetch = index_listed.expect_single_fetch();
    assert!(
        index_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30&page=1"),
        "unexpected runs listing URL: {}",
        index_fetch.url
    );

    index_listed
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"{"workflow_runs":[{"id":42,"status":"in_progress","conclusion":null}]}"#
                .to_vec(),
            redirected: None,
        })])
        .unwrap();
    match index_listed.result().unwrap() {
        Ok(ListChildrenResult::Entries(_)) => {
            assert_eq!(
                canonical_bytes(index_listed.effects().unwrap()),
                [br#"{"id":42,"status":"in_progress","conclusion":null}"#.as_slice()]
            );
        },
        other => panic!("expected runs listing with its canonical rows, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_resource_reads_do_not_fall_back_to_provider_cache() {
    use omnifs_wit::host::types::{CalloutError, CalloutResult, ErrorKind, Header, HttpResponse};

    struct Case {
        name: &'static str,
        path: &'static str,
        ok_headers: Vec<Header>,
        ok_body: &'static [u8],
        expected_content: &'static [u8],
    }

    // PR diff is covered separately by `github_pr_lookup_validates_and_exposes_diff`
    // because it dispatches a fetch-blob callout (and returns a blob-backed
    // ReadFileBytes) rather than an inline HttpResponse.
    //
    // Object field files are covered by item-specific tests. These structural
    // file routes must still work from a cold read path without relying on a
    // previous directory listing to preload content into the host cache.
    let cases = [
        Case {
            // A comment is a `{comment_id}` object dir; `comment.md` renders the
            // standalone comment GET. Reading it fetches the comment from
            // upstream rather than serving a provider-side cache.
            name: "issue comment",
            path: "/octocat/Hello-World/issues/open/1/comments/1/comment.md",
            ok_headers: vec![Header {
                name: "etag".to_string(),
                value: "\"comment-1\"".to_string(),
            }],
            ok_body: br#"{"id":1,"user":{"login":"octocat"},"body":"A comment"}"#,
            expected_content: b"octocat:\nA comment\n",
        },
        Case {
            name: "action status",
            path: "/octocat/Hello-World/actions/runs/99/status",
            ok_headers: Vec::new(),
            ok_body: br#"{"id":99,"status":"completed","conclusion":"success"}"#,
            expected_content: b"completed",
        },
        Case {
            name: "action conclusion",
            path: "/octocat/Hello-World/actions/runs/99/conclusion",
            ok_headers: Vec::new(),
            ok_body: br#"{"id":99,"status":"completed","conclusion":"success"}"#,
            expected_content: b"success",
        },
    ];

    let harness = github_harness();
    for case in &cases {
        let mut first = harness.read(case.path).unwrap();
        assert!(
            first.is_waiting_for_callouts(),
            "{name}: expected fetch callout on first read, got {first:?}",
            name = case.name
        );
        first
            .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
                status: 200,
                headers: case.ok_headers.clone(),
                body: case.ok_body.to_vec(),
                redirected: None,
            })])
            .unwrap();
        match first.result().unwrap() {
            Ok(ReadFileOutcome::Found(file)) => {
                if !case.expected_content.is_empty() {
                    assert_eq!(
                        omnifs_itest::expect_inline(file),
                        case.expected_content,
                        "{name}: unexpected cached content",
                        name = case.name
                    );
                }
            },
            other => panic!("{}: expected ReadFile result, got {other:?}", case.name),
        }

        let mut second = harness.read(case.path).unwrap();
        assert!(
            second.is_waiting_for_callouts(),
            "{name}: expected fetch callout on second read (no provider cache), got {second:?}",
            name = case.name
        );
        second
            .answer_callouts(vec![CalloutResult::CalloutError(CalloutError {
                kind: ErrorKind::Network,
                message: "network down".to_string(),
                retryable: true,
            })])
            .unwrap();
        match second.result().unwrap() {
            Err(err) => {
                assert_eq!(
                    err.kind,
                    ErrorKind::Network,
                    "{}: wrong error kind",
                    case.name
                );
            },
            other => panic!(
                "{}: expected Network error on second read, got {other:?}",
                case.name
            ),
        }
    }
}

/// `actions/runs/{id}/logs.zip` is the archive of the logs of the run as a
/// blob that the host holds, so that no size bounds it: its fetch follows the
/// redirect of GitHub to the storage of the archive, and a status that is no
/// success is the error of the read.
#[test]
fn github_action_logs_are_the_archive_that_the_host_holds() {
    use omnifs_wit::host::types::{BlobContent, BlobFetched, ByteSource, Callout, ErrorKind};

    const LOGS: &str = "/octocat/Hello-World/actions/runs/99/logs.zip";
    let harness = github_harness();
    let fetch = |status: u16, content: Option<BlobContent>| {
        let mut read = harness.read(LOGS).unwrap();
        let [Callout::FetchBlob(request)] = read.callouts() else {
            panic!(
                "expected the blob fetch of the logs, got {:?}",
                read.callouts()
            );
        };
        assert!(
            request
                .url
                .ends_with("/repos/octocat/Hello-World/actions/runs/99/logs"),
            "unexpected logs URL: {}",
            request.url
        );
        assert!(request.follow_redirects);
        assert_eq!(request.version, None);
        read.answer_callouts(vec![CalloutResult::BlobFetched(BlobFetched {
            status,
            response_headers: Vec::new(),
            redirected: Some("https://storage.example/logs.zip".to_string()),
            upstream: true,
            content,
        })])
        .unwrap();
        read.into_ok()
    };
    match fetch(
        200,
        Some(BlobContent {
            blob: 9,
            size: 4_096,
        }),
    )
    .unwrap()
    {
        ReadFileOutcome::Found(file) => {
            assert!(
                matches!(file.bytes, ByteSource::Blob(9)),
                "{:?}",
                file.bytes
            );
            assert_eq!(file.attrs.stability, Stability::Dynamic);
        },
        other => panic!("expected the archive, got {other:?}"),
    }
    match fetch(410, None) {
        Err(omnifs_engine::EngineError::ProviderError(error)) => {
            assert_eq!(error.kind, ErrorKind::InvalidInput);
        },
        other => panic!("expected the error of a gone archive, got {other:?}"),
    }
}

/// Comments are `{comment_id}` object directories, fed by the feeds of their
/// repository and item. Each `comment.json` / `comment.md` face loads from
/// the standalone comment GET. This test pins (a) the id-keyed dir model,
/// (b) the refetch invariant: a comment face read fetches upstream and a
/// reread refetches (never serves a provider-side cache), and (c) reject: a
/// face under a missing comment validates to not-found on read.
#[test]
#[allow(clippy::too_many_lines)]
fn github_provider_comment_routes_id_dirs_and_refetch() {
    use omnifs_wit::host::types::{CalloutError, CalloutResult, Header};

    fn ok_body(body: &[u8]) -> Vec<CalloutResult> {
        vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"c\"".to_string(),
            }],
            body: body.to_vec(),
            redirected: None,
        })]
    }

    let harness = github_harness();

    // The fed comment anchor lists its faces without a load of the comment.
    let comment_dir = "/octocat/Hello-World/issues/open/1/comments/42";
    let dir = harness.list(comment_dir).unwrap();
    assert!(dir.callouts().is_empty());
    match dir.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let mut names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            names.sort_unstable();
            assert_eq!(
                names,
                vec!["author", "body.md", "comment.json", "comment.md"]
            );
        },
        other => panic!("expected comment dir listing, got {other:?}"),
    }

    // `comment.md` renders the standalone comment GET, keyed by comment id.
    let comment_md = "/octocat/Hello-World/issues/open/1/comments/42/comment.md";
    let mut first = harness.read(comment_md).unwrap();
    let first_fetch = first.expect_single_fetch();
    assert!(
        first_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/42"),
        "unexpected comment fetch URL: {}",
        first_fetch.url
    );
    first
        .answer_callouts(ok_body(
            br#"{"id":42,"user":{"login":"octocat"},"body":"A comment"}"#,
        ))
        .unwrap();
    match first.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(omnifs_itest::expect_inline(file), b"octocat:\nA comment\n");
        },
        other => panic!("expected comment.md content, got {other:?}"),
    }

    // Refetch invariant: a reread does NOT serve a provider-side cache; it
    // fetches again. A network error on the refetch surfaces as Network.
    let mut reread = harness.read(comment_md).unwrap();
    assert!(
        reread.is_waiting_for_callouts(),
        "comment reread should refetch, got {reread:?}"
    );
    reread
        .answer_callouts(vec![CalloutResult::CalloutError(CalloutError {
            kind: omnifs_wit::host::types::ErrorKind::Network,
            message: "network down".to_string(),
            retryable: true,
        })])
        .unwrap();
    match reread.result().unwrap() {
        Err(error) => {
            assert_eq!(error.kind, omnifs_wit::host::types::ErrorKind::Network);
        },
        other => panic!("expected Network error on comment refetch, got {other:?}"),
    }

    // `comment.json` serves the canonical comment bytes verbatim and refetches
    // identically on a cold reread.
    let comment_json = "/octocat/Hello-World/issues/open/1/comments/42/comment.json";
    let mut json_read = harness.read(comment_json).unwrap();
    assert!(json_read.is_waiting_for_callouts());
    json_read
        .answer_callouts(ok_body(
            br#"{"id":42,"user":{"login":"octocat"},"body":"A comment"}"#,
        ))
        .unwrap();
    match json_read.result().unwrap() {
        Ok(ReadFileOutcome::Found(_)) => {},
        other => panic!("expected comment.json read, got {other:?}"),
    }

    // PR comments use the same id-keyed dir model and the same standalone
    // comment GET (comments live under the shared issues endpoint).
    let pr_comment_md = "/octocat/Hello-World/pulls/open/7/comments/9/comment.md";
    let mut pr_read = harness.read(pr_comment_md).unwrap();
    let pr_fetch = pr_read.expect_single_fetch();
    assert!(
        pr_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/9"),
        "unexpected PR comment fetch URL: {}",
        pr_fetch.url
    );
    pr_read
        .answer_callouts(ok_body(
            br#"{"id":9,"user":{"login":"hubot"},"body":"a pr comment"}"#,
        ))
        .unwrap();
    match pr_read.result().unwrap() {
        Ok(ReadFileOutcome::Found(file)) => {
            assert_eq!(file.attrs.stability, Stability::Dynamic);
            assert_eq!(omnifs_itest::expect_inline(file), b"hubot:\na pr comment\n");
        },
        other => panic!("expected PR comment content, got {other:?}"),
    }

    // Reject: a face under a missing comment id validates on read. A 404 on the
    // comment object load resolves the object as not-found.
    let mut missing = harness
        .read("/octocat/Hello-World/issues/open/1/comments/999/comment.json")
        .unwrap();
    let missing_fetch = missing.expect_single_fetch();
    assert!(
        missing_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/999"),
        "unexpected missing-comment fetch URL: {}",
        missing_fetch.url
    );
    missing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 404,
            headers: Vec::new(),
            body: b"{\"message\":\"Not Found\"}".to_vec(),
            redirected: None,
        })])
        .unwrap();
    match missing.result().unwrap() {
        Ok(ReadFileOutcome::NotFound(_)) => {},
        other => panic!("expected NotFound on missing comment, got {other:?}"),
    }
}

#[test]
fn github_owner_listing_asks_for_the_profile_once_and_the_repos_of_an_organization_first() {
    use omnifs_wit::host::types::{CalloutResult, HttpResponse};

    let harness = github_harness();
    // lookup_child on an owner segment resolves structurally without a fetch.
    // The owner is an implicit prefix dir (/{owner}/{repo} routes exist under it).
    let lookup = harness.lookup("/", "openai").unwrap();
    match lookup.result().unwrap() {
        Ok(LookupChildResult::Entry(result)) => {
            assert_eq!(result.target.name, "openai");
            assert!(
                matches!(result.target.kind, EntryKind::Directory),
                "owner anchor should be a directory"
            );
        },
        other => panic!("expected immediate Dir entry for owner, got {other:?}"),
    }

    // Listing the owner object loads its canonical (owner.json/profile.md
    // faces) from `/users/{login}`, which answers an organization too, once.
    // The first page of its repos then asks the repos of an organization.
    let mut listing = harness.list("/openai").unwrap();
    let load = listing.expect_single_fetch();
    assert!(
        load.url.ends_with("/users/openai"),
        "expected the owner profile, got {}",
        load.url
    );
    listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: br#"{"login":"openai","type":"Organization","name":null,"bio":null}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    let repos_fetch = listing.expect_single_fetch();
    assert!(
        repos_fetch
            .url
            .ends_with("/orgs/openai/repos?sort=full_name&direction=asc&per_page=100&page=1"),
        "expected the repos of the organization after its profile, got {}",
        repos_fetch.url
    );

    listing
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: br#"[{"name":"api"}]"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    match listing.result().unwrap() {
        Ok(ListChildrenResult::Entries(result)) => {
            let names: Vec<&str> = result
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert!(
                names.contains(&"api"),
                "expected api in owner listing, got {names:?}"
            );
            assert!(result.next_cursor.is_none());
            assert_eq!(
                result.validator.as_ref().map(|page| page.token.as_str()),
                Some(r#"{"kind":"org","etag":"\"page-1\""}"#)
            );
        },
        other => panic!("expected owner listing result, got {other:?}"),
    }

    // Root does not enumerate owners: it exposes only the generated README
    // and the notifications literal. Browsed owners do not leak into it.
    let root_listing = harness.list("/").unwrap();
    match root_listing.result().unwrap() {
        Ok(ListChildrenResult::Entries(listing)) => {
            let names: Vec<&str> = listing
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect();
            assert_eq!(names, vec!["README.md", "notifications"]);
        },
        other => panic!("expected root listing, got {other:?}"),
    }
}

#[test]
fn github_provider_polls_events_and_invalidates_caches() {
    // State is empty and timer ticks are no-ops. This test verifies both the
    // comment read path and the absence of timer callouts or invalidations.
    let harness = github_harness();
    // Read a comment object face: a `{comment_id}` dir's `comment.json` loads the
    // standalone comment GET.
    let comment_path = "/octocat/Hello-World/issues/open/1/comments/1/comment.json";

    let mut issue_cached = harness.read(comment_path).unwrap();
    let issue_fetch = issue_cached.expect_single_fetch();
    assert!(
        issue_fetch
            .url
            .ends_with("/repos/octocat/Hello-World/issues/comments/1"),
        "unexpected comment fetch URL: {}",
        issue_fetch.url
    );
    issue_cached
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![Header {
                name: "etag".to_string(),
                value: "\"comment-1\"".to_string(),
            }],
            body: br#"{"id":1,"user":{"login":"octocat"},"body":"A comment"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
    match issue_cached.result().unwrap() {
        Ok(_) => {},
        other => panic!("expected comment ReadFile result, got {other:?}"),
    }
}

#[test]
fn github_provider_list_routes_preserve_typed_http_errors() {
    use omnifs_wit::host::types::{CalloutResult, ErrorKind, Header, HttpResponse};

    let harness = github_harness();
    let mut op = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    let fetch = op.expect_single_fetch();
    assert!(
        fetch
            .url
            .ends_with("/repos/octocat/Hello-World/actions/runs?per_page=30&page=1"),
        "unexpected URL {}",
        fetch.url
    );
    op.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 403,
        headers: vec![Header {
            name: "etag".to_string(),
            value: "\"denied\"".to_string(),
        }],
        body: br#"{"message":"forbidden"}"#.to_vec(),
        redirected: None,
    })])
    .unwrap();
    match op.result().unwrap() {
        Err(error) => assert_eq!(error.kind, ErrorKind::Denied),
        other => panic!("expected provider error result, got {other:?}"),
    }
}

/// The load of `item.json` of an item, which a read of it without the facts
/// of its feed asks, is the node of GitHub GraphQL for its number verbatim:
/// the bytes that the feeds write for it. An item that GitHub answers from
/// another repository for the number is not found.
#[test]
fn item_json_is_the_node_of_its_number() {
    use omnifs_wit::host::types::{
        ByteSource, Callout, CalloutResult, HttpResponse, ReadFileOutcome,
    };

    let item_json = r#"{"number":42,"title":"Item forty-two","body":"Body text","state":"OPEN","author":{"login":"octocat"},"updatedAt":"2026-11-18T09:00:00Z","comments":{"totalCount":0}}"#;
    let answer = |kind: &str, repository: &str| {
        format!(
            r#"{{"data":{{"repository":{{"nameWithOwner":"octocat/Hello-World",
            "t42":{{"__typename":"{kind}","number":42,"repository":{{"nameWithOwner":"{repository}"}}}},
            "n42":{item_json}}}}}}}"#
        )
    };
    let harness = github_harness();
    for (kind, typename) in [("issues", "Issue"), ("pulls", "PullRequest")] {
        let mut step = harness
            .read(&format!("/octocat/Hello-World/{kind}/all/42/item.json"))
            .unwrap();
        let [Callout::Fetch(fetch)] = step.callouts() else {
            panic!("expected item fetch, got {step:?}");
        };
        assert_eq!(fetch.url, "https://api.github.com/graphql");
        let query = String::from_utf8(fetch.body.clone().unwrap()).unwrap();
        assert!(
            query.contains("t42: issueOrPullRequest(number: 42)"),
            "{query}"
        );
        step.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: answer(typename, "octocat/Hello-World").into_bytes(),
            redirected: None,
        })])
        .unwrap();
        let Ok(ReadFileOutcome::Found(file)) = step.result().unwrap() else {
            panic!("expected read terminal, got {step:?}");
        };
        assert_eq!(
            step.effects().unwrap().canonical[0].bytes,
            item_json.as_bytes()
        );
        match &file.bytes {
            ByteSource::Canonical => {},
            ByteSource::Inline(bytes) => assert_eq!(bytes.as_slice(), item_json.as_bytes()),
            other => panic!("expected canonical or inline item.json bytes, got {other:?}"),
        }
    }

    let mut moved = harness
        .read("/octocat/Hello-World/issues/all/42/item.json")
        .unwrap();
    moved.expect_single_fetch();
    moved
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: answer("Issue", "octocat/elsewhere").into_bytes(),
            redirected: None,
        })])
        .unwrap();
    assert!(
        matches!(moved.result().unwrap(), Ok(ReadFileOutcome::NotFound(_))),
        "an item of another repository is not this one's"
    );
}

/// The canonical bytes that `effects` store, each of its object.
fn canonical_bytes(effects: &omnifs_wit::host::types::Effects) -> Vec<&[u8]> {
    effects
        .canonical
        .iter()
        .map(|canonical| canonical.bytes.as_slice())
        .collect()
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: name.to_string(),
        value: value.to_string(),
    }
}

fn entry_names(page: &omnifs_engine::DirPage) -> Vec<&str> {
    page.entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect()
}

/// A page of `actions/runs/` with the `ETag` of its answer serves from cache
/// within its deadline, three seconds without an interval of its upstream, and
/// is asked for again with `If-None-Match` only after it; `304` keeps it, a
/// rate limit is the error of the listing, and a read in the window of the
/// rate limit takes the cached entries without a request.
#[tokio::test(flavor = "multi_thread")]
async fn github_action_runs_page_is_asked_again_with_its_etag_after_its_deadline() {
    let harness = github_harness();
    let runs = resolve_namespace(
        harness.namespace.as_ref(),
        "/github/octocat/Hello-World/actions/runs",
    )
    .await;
    let readdir = || {
        harness
            .namespace
            .readdir(runs.path.clone(), omnifs_engine::DirCursor::start(), 0)
    };

    let (first, requests) = answering(
        &harness,
        readdir(),
        [fetched(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""runs-1""#)],
            body: br#"{"workflow_runs":[{"id":123,"status":"completed","conclusion":"success"}]}"#
                .to_vec(),
            redirected: None,
        })],
    )
    .await;
    assert_eq!(entry_names(&first.unwrap()), ["123"]);
    assert!(
        http(&requests[0])
            .url
            .ends_with("/actions/runs?per_page=30&page=1")
    );
    assert_eq!(request_header(http(&requests[0]), "if-none-match"), None);

    let (cached, requests) = answering(&harness, readdir(), []).await;
    assert_eq!(entry_names(&cached.unwrap()), ["123"]);
    assert!(requests.is_empty(), "the page serves within its deadline");

    tokio::time::sleep(std::time::Duration::from_millis(3_300)).await;
    let (kept, requests) = answering(&harness, readdir(), [fetched(not_modified())]).await;
    assert_eq!(entry_names(&kept.unwrap()), ["123"]);
    assert_eq!(
        request_header(http(&requests[0]), "if-none-match"),
        Some(r#""runs-1""#)
    );

    tokio::time::sleep(std::time::Duration::from_millis(3_300)).await;
    let (limited, _) = answering(
        &harness,
        readdir(),
        [fetched(HttpResponse {
            status: 429,
            headers: vec![header("Retry-After", "60")],
            body: br#"{"message":"API rate limit exceeded"}"#.to_vec(),
            redirected: None,
        })],
    )
    .await;
    assert!(matches!(
        limited,
        Err(omnifs_engine::NsError::RateLimited { .. })
    ));

    let (windowed, requests) = answering(&harness, readdir(), []).await;
    assert_eq!(entry_names(&windowed.unwrap()), ["123"]);
    assert!(
        requests.is_empty(),
        "a read in the window of the rate limit asks GitHub nothing"
    );
}

/// A page answer without an `ETag` has no validator to be asked for again
/// with, and `notifications/` takes the seconds of `X-Poll-Interval` only as
/// a number: each is an error of the listing.
#[test]
fn github_page_answers_without_an_etag_or_with_another_poll_interval_fail() {
    let harness = github_harness();
    let mut runs = harness.list("/octocat/Hello-World/actions/runs").unwrap();
    runs.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: br#"{"workflow_runs":[]}"#.to_vec(),
        redirected: None,
    })])
    .unwrap();
    assert!(matches!(
        runs.result().unwrap(),
        Err(error) if error.message.contains("without an ETag")
    ));

    let mut notifications = harness.list("/notifications").unwrap();
    notifications
        .answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 200,
            headers: vec![
                header("ETag", r#""notifications-1""#),
                header("X-Poll-Interval", "soon"),
            ],
            body: b"[]".to_vec(),
            redirected: None,
        })])
        .unwrap();
    assert!(matches!(
        notifications.result().unwrap(),
        Err(error) if error.message.contains("X-Poll-Interval")
    ));
}

/// `notifications/` is asked for again with `If-None-Match` only after the
/// seconds of the `X-Poll-Interval` of its answer, here one second, shorter
/// than the deadline of a page without an interval.
#[tokio::test(flavor = "multi_thread")]
async fn github_notifications_are_asked_again_only_after_their_poll_interval() {
    let harness = github_harness();
    let notifications =
        resolve_namespace(harness.namespace.as_ref(), "/github/notifications").await;
    let readdir = || {
        harness.namespace.readdir(
            notifications.path.clone(),
            omnifs_engine::DirCursor::start(),
            0,
        )
    };

    let (first, requests) = answering(
        &harness,
        readdir(),
        [fetched(HttpResponse {
            status: 200,
            headers: vec![
                header("ETag", r#""notifications-1""#),
                header("X-Poll-Interval", "1"),
            ],
            body: br#"[{
                "id":"123",
                "unread":true,
                "reason":"mention",
                "updated_at":"2026-07-05T00:00:00Z",
                "subject":{"title":"Review requested","type":"PullRequest"},
                "repository":{"full_name":"octocat/Hello-World"}
            }]"#
            .to_vec(),
            redirected: None,
        })],
    )
    .await;
    assert_eq!(entry_names(&first.unwrap()), ["README.md", "thread-123"]);
    assert!(
        http(&requests[0])
            .url
            .ends_with("/notifications?per_page=50&page=1")
    );

    let (_, requests) = answering(&harness, readdir(), []).await;
    assert!(
        requests.is_empty(),
        "the page serves within its poll interval"
    );

    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    let (kept, requests) = answering(
        &harness,
        readdir(),
        [fetched(HttpResponse {
            status: 304,
            headers: vec![header("X-Poll-Interval", "1")],
            body: Vec::new(),
            redirected: None,
        })],
    )
    .await;
    assert_eq!(entry_names(&kept.unwrap()), ["README.md", "thread-123"]);
    assert_eq!(
        request_header(http(&requests[0]), "if-none-match"),
        Some(r#""notifications-1""#)
    );

    let (_, requests) = answering(&harness, readdir(), []).await;
    assert!(requests.is_empty(), "304 renews the deadline of the page");
}

/// Pull request 7 by its number, which its own feed asks GitHub GraphQL for
/// before the first full listing of the repository, and again once the feed
/// is past its deadline.
fn pull_seven() -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: Vec::new(),
        body: br#"{"data":{"repository":{
            "nameWithOwner":"octocat/Hello-World",
            "t7":{"__typename":"PullRequest","number":7,"repository":{"nameWithOwner":"octocat/Hello-World"}},
            "n7":{"number":7,"title":"Seven","body":"Seven body","state":"OPEN","author":{"login":"octocat"},
                  "updatedAt":"2026-11-18T09:00:00Z","comments":{"totalCount":0}}
        }}}"#
        .to_vec(),
        redirected: None,
    }
}

/// The page of the folder `folder` of pull request 7, `body` from `url` with
/// the `ETag` `"page-1"`, has `names`. It serves from cache within its
/// deadline, three seconds without an interval of its upstream, and is asked
/// for again with `If-None-Match` only after it, once the feed of the pull
/// request has asked for its row again; `304` keeps it.
async fn pull_page_is_asked_again_with_its_etag_after_its_deadline(
    folder: &str,
    url: &str,
    body: &[u8],
    names: &[&str],
) {
    let harness = github_harness();
    let (dir, _) = answering(
        &harness,
        resolve_namespace(
            harness.namespace.as_ref(),
            &format!("/github/octocat/Hello-World/pulls/open/7/{folder}"),
        ),
        [fetched(pull_seven())],
    )
    .await;
    let readdir = || {
        harness
            .namespace
            .readdir(dir.path.clone(), omnifs_engine::DirCursor::start(), 0)
    };

    let (first, requests) = answering(
        &harness,
        readdir(),
        [fetched(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: body.to_vec(),
            redirected: None,
        })],
    )
    .await;
    assert_eq!(entry_names(&first.unwrap()), names);
    assert_eq!(
        http(&requests[0]).url,
        format!("https://api.github.com/repos/octocat/Hello-World/pulls/7/{url}")
    );
    assert_eq!(request_header(http(&requests[0]), "if-none-match"), None);

    let (cached, requests) = answering(&harness, readdir(), []).await;
    assert_eq!(entry_names(&cached.unwrap()), names);
    assert!(requests.is_empty(), "the page serves within its deadline");

    tokio::time::sleep(std::time::Duration::from_millis(3_300)).await;
    let (kept, requests) = answering(
        &harness,
        readdir(),
        [fetched(pull_seven()), fetched(not_modified())],
    )
    .await;
    assert_eq!(entry_names(&kept.unwrap()), names);
    assert_eq!(http(&requests[0]).url, "https://api.github.com/graphql");
    assert_eq!(
        http(&requests[1]).url,
        format!("https://api.github.com/repos/octocat/Hello-World/pulls/7/{url}")
    );
    assert_eq!(
        request_header(http(&requests[1]), "if-none-match"),
        Some(r#""page-1""#)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn github_pr_files_page_is_asked_again_with_its_etag_after_its_deadline() {
    pull_page_is_asked_again_with_its_etag_after_its_deadline(
        "files",
        "files?per_page=100&page=1",
        br#"[{"filename":"src/lib.rs","status":"modified","additions":1,"deletions":1,"changes":2}]"#,
        &["src%2Flib.rs"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn github_pr_reviews_page_is_asked_again_with_its_etag_after_its_deadline() {
    pull_page_is_asked_again_with_its_etag_after_its_deadline(
        "reviews",
        "reviews?per_page=100&page=1",
        br#"[{"id":80,"state":"APPROVED","body":"ok","user":{"login":"reviewer"}}]"#,
        &["80"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn github_pr_review_comments_page_is_asked_again_with_its_etag_after_its_deadline() {
    pull_page_is_asked_again_with_its_etag_after_its_deadline(
        "reviews/80/comments",
        "reviews/80/comments?per_page=100&page=1",
        br#"[{"id":99,"body":"nit","path":"src/lib.rs","diff_hunk":"@@ -1 +1 @@","user":{"login":"reviewer"}}]"#,
        &["99"],
    )
    .await;
}

/// The files of an entry of a listing render from the row that the listing
/// brought, without a request of their own: the files, the reviews, and the
/// check runs of a pull request, and the workflow runs of a repository.
#[tokio::test(flavor = "multi_thread")]
async fn the_files_of_a_listed_entry_render_from_its_row_without_a_request() {
    use omnifs_itest::support::{names, read, served};

    let harness = github_harness();
    let page = |body: &str| {
        fetched(HttpResponse {
            status: 200,
            headers: vec![header("ETag", r#""page-1""#)],
            body: body.as_bytes().to_vec(),
            redirected: None,
        })
    };
    let pull = "/github/octocat/Hello-World/pulls/open/7";
    let file =
        r#"{"filename":"src/lib.rs","status":"modified","additions":1,"deletions":1,"changes":2}"#;
    let review = r#"{"id":80,"state":"APPROVED","body":"ok","user":{"login":"reviewer"}}"#;
    let check = r#"{"id":700,"name":"ci","status":"completed","conclusion":"success","output":{"title":null,"summary":null,"text":null}}"#;
    let run = r#"{"id":123,"status":"completed","conclusion":"success"}"#;
    let (listing, _) = answering(
        &harness,
        names(&harness, &format!("{pull}/files")),
        [fetched(pull_seven()), page(&format!("[{file}]"))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["src%2Flib.rs"]);
    let (listing, _) = answering(
        &harness,
        names(&harness, &format!("{pull}/reviews")),
        [page(&format!("[{review}]"))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["80"]);
    let (listing, _) = answering(
        &harness,
        names(&harness, &format!("{pull}/checks")),
        [
            page(r#"{"base":{"sha":"base1"},"head":{"sha":"abc123"}}"#),
            page(&format!(r#"{{"check_runs":[{check}]}}"#)),
        ],
    )
    .await;
    assert_eq!(listing.unwrap(), ["700"]);
    let (listing, _) = answering(
        &harness,
        names(&harness, "/github/octocat/Hello-World/actions/runs"),
        [page(&format!(r#"{{"workflow_runs":[{run}]}}"#))],
    )
    .await;
    assert_eq!(listing.unwrap(), ["123"]);

    for (path, expected) in [
        (format!("{pull}/files/src%2Flib.rs/status"), "modified"),
        (format!("{pull}/files/src%2Flib.rs/file.json"), file),
        (format!("{pull}/reviews/80/state"), "APPROVED"),
        (format!("{pull}/reviews/80/review.json"), review),
        (format!("{pull}/checks/700/name"), "ci"),
        (format!("{pull}/checks/700/check.json"), check),
        (
            "/github/octocat/Hello-World/actions/runs/123/conclusion".to_owned(),
            "success",
        ),
        (
            "/github/octocat/Hello-World/actions/runs/123/run.json".to_owned(),
            run,
        ),
    ] {
        assert_eq!(
            served(&harness, read(&harness, &path)).await.unwrap(),
            expected,
            "{path}"
        );
    }
}

/// The `301` of a repository that moved, and of a comment of an issue that
/// moved, is not followed: the object is not at its path.
#[test]
fn an_object_that_moved_is_not_at_its_path() {
    use omnifs_wit::host::types::{CalloutResult, HttpResponse, ReadFileOutcome};

    let harness = github_harness();
    for (path, url) in [
        (
            "/octocat/Hello-World/repo.json",
            "/repos/octocat/Hello-World",
        ),
        (
            "/octocat/Hello-World/issues/all/7/comments/70/comment.json",
            "/repos/octocat/Hello-World/issues/comments/70",
        ),
    ] {
        let mut read = harness.read(path).unwrap();
        let fetch = read.expect_single_fetch();
        assert!(fetch.url.ends_with(url), "{}", fetch.url);
        assert!(!fetch.follow_redirects, "{path}");
        read.answer_callouts(vec![CalloutResult::HttpResponse(HttpResponse {
            status: 301,
            headers: vec![header("Location", "https://api.github.com/repositories/1")],
            body: br#"{"message":"Moved Permanently"}"#.to_vec(),
            redirected: None,
        })])
        .unwrap();
        assert!(
            matches!(read.result().unwrap(), Ok(ReadFileOutcome::NotFound(_))),
            "{path}"
        );
    }
}
