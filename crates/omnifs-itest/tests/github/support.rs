//! GitHub provider route-test helpers.

use omnifs_core::path::Path;
use omnifs_engine::{LookupAnswer, Namespace};
use omnifs_itest::support::{Answer, Request};
use omnifs_itest::{RuntimeHarness, make_initialized_runtime};
use omnifs_wit::host::types::{
    CalloutResult, Effects, FsKind, HttpRequest, HttpResponse, Stability,
};

pub use omnifs_itest::{TestOpExt, project_paths};

pub async fn resolve_namespace(namespace: &dyn Namespace, path: &str) -> LookupAnswer {
    let attrs = namespace.getattr(Path::root()).await.unwrap();
    let mut answer = LookupAnswer::found(Path::root(), attrs);
    for segment in Path::parse(path).unwrap().segments() {
        answer = namespace.lookup(answer.path, segment).await.unwrap();
    }
    answer
}

pub fn github_harness() -> RuntimeHarness {
    make_initialized_runtime(
        r#"
        {
            "provider": "omnifs_provider_github.wasm",
            "mount": "github",
            "auth": {
                "type": "static-token",
                "scheme": "pat"
            }
        }
    "#,
    )
}

pub fn project_file_stability(effects: &Effects, path: &str) -> Option<Stability> {
    effects.fs.iter().find_map(|write| {
        if write.path != path {
            return None;
        }
        match &write.kind {
            FsKind::File(file) => Some(file.attrs.stability),
            FsKind::Directory(_) => None,
        }
    })
}

/// The request of a fetch that `answering` answered.
pub fn http(request: &Request) -> &HttpRequest {
    match request {
        Request::Fetch(request) => request,
        Request::Blob(request) => panic!("a blob fetch of {}", request.url),
    }
}

/// The answer of a fetch with `response`.
pub fn fetched(response: HttpResponse) -> Answer {
    Answer::Fetch(CalloutResult::HttpResponse(response))
}

/// An answer `304` without headers.
pub fn not_modified() -> HttpResponse {
    HttpResponse {
        status: 304,
        headers: Vec::new(),
        body: Vec::new(),
        redirected: None,
    }
}

/// The value of header `name` of `request`.
pub fn request_header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}
