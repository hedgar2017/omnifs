//! Stable, domain-separated identities for host-owned callout storage.

use omnifs_auth::CredentialId;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct BlobRequestId([u8; 32]);

/// Identity for a mount-scoped Git checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub(crate) struct GitId([u8; 32]);

/// Stable identity of the projection of a mount: its source, the canonical
/// mount with the number of the projection of its provider in place of its
/// artifact, and its credential generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ProjectionId([u8; 32]);

impl ProjectionId {
    #[must_use]
    pub(crate) fn new(spec_source: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        frame(&mut hasher, b"omnifs/projection/v3");
        frame(&mut hasher, spec_source);
        Self(*hasher.finalize().as_bytes())
    }

    #[must_use]
    pub(crate) fn hex(self) -> String {
        hex(self.0)
    }
}

impl BlobRequestId {
    /// Hash the validated facts of a blob request: the stable credential
    /// partition, the method, the canonical URL, the headers and the body
    /// of the provider, whether the request follows redirects, and the
    /// identity of the bytes that the provider asks for, by their MD5, by
    /// their version, by both, or by neither. Token bytes and refresh
    /// generations never enter this identity.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        auth: Option<&CredentialId>,
        method: &str,
        canonical_url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        follow_redirects: bool,
        md5: Option<&str>,
        version: Option<&str>,
    ) -> Self {
        let auth_partition = auth.map(ToString::to_string);
        let headers = encode_headers(headers);
        Self(hash_parts(
            b"omnifs/blob-request/v2",
            &[
                auth_partition
                    .as_deref()
                    .unwrap_or("unauthenticated")
                    .as_bytes(),
                method.as_bytes(),
                canonical_url.as_bytes(),
                &headers,
                &optional(body),
                &[u8::from(follow_redirects)],
                &optional(md5.map(str::as_bytes)),
                &optional(version.map(str::as_bytes)),
            ],
        ))
    }

    pub(crate) fn filesystem_name(self) -> String {
        hex(self.0)
    }
}

/// `value` apart from none: an empty value is another part than none.
fn optional(value: Option<&[u8]>) -> Vec<u8> {
    let mut material = Vec::new();
    match value {
        Some(bytes) => {
            material.push(1);
            frame_bytes(&mut material, bytes);
        },
        None => material.push(0),
    }
    material
}

impl GitId {
    pub(crate) fn new(mount_scope: &str, canonical_remote: &str, reference: Option<&str>) -> Self {
        let reference_material = optional(reference.map(str::as_bytes));
        Self(hash_parts(
            b"omnifs/git/v1",
            &[
                mount_scope.as_bytes(),
                canonical_remote.as_bytes(),
                &reference_material,
            ],
        ))
    }

    pub(crate) fn filesystem_name(&self) -> String {
        hex(self.0)
    }
}

impl fmt::Display for BlobRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.filesystem_name())
    }
}

impl fmt::Display for GitId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.filesystem_name())
    }
}

fn hash_parts(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    frame(&mut hasher, domain);
    for part in parts {
        frame(&mut hasher, part);
    }
    *hasher.finalize().as_bytes()
}

fn frame(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn encode_headers(headers: &[(String, String)]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in headers {
        frame_bytes(&mut encoded, name.as_bytes());
        frame_bytes(&mut encoded, value.as_bytes());
    }
    encoded
}

fn frame_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn hex(bytes: [u8; 32]) -> String {
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(
        auth: Option<&CredentialId>,
        method: &str,
        url: &str,
        body: Option<&[u8]>,
    ) -> BlobRequestId {
        BlobRequestId::new(auth, method, url, &[], body, false, None, None)
    }

    #[test]
    fn identities_are_unambiguous_and_mount_scoped() {
        assert_ne!(
            blob(None, "GET", "https://example.test/A", None),
            blob(None, "GET", "https://example.test/a", None),
        );
        assert_ne!(
            blob(None, "POST", "https://example.test", None),
            blob(None, "POST", "https://example.test", Some(b"")),
        );
        assert_ne!(
            GitId::new("mount-a", "remote", Some("main")),
            GitId::new("mount-b", "remote", Some("main")),
        );
        assert_ne!(
            GitId::new("mount", "remote", None),
            GitId::new("mount", "remote", Some("")),
        );
    }

    #[test]
    fn credential_partitions_reuse_without_cross_account_sharing() {
        let first = CredentialId::new("github", "oauth", "alice").unwrap();
        let second = CredentialId::new("github", "oauth", "bob").unwrap();
        let same_request = |auth| blob(auth, "GET", "https://api.example.test/data", None);

        assert_eq!(same_request(Some(&first)), same_request(Some(&first)));
        assert_ne!(same_request(Some(&first)), same_request(Some(&second)));
        assert_ne!(same_request(None), same_request(Some(&first)));
    }

    /// The MD5 and the version of a request are parts of its identity of
    /// their own, and so is whether it follows redirects: the answer of a
    /// redirect is of another URL.
    #[test]
    fn the_identity_of_the_bytes_and_the_redirects_are_parts_of_the_identity() {
        let request = |follow, md5, version| {
            BlobRequestId::new(
                None,
                "GET",
                "https://files.example.test/file",
                &[],
                None,
                follow,
                md5,
                version,
            )
        };
        let md5 = "3d93da86c9045cb1b17e96790cd19789";
        let all = [
            request(false, None, None),
            request(true, None, None),
            request(false, Some(md5), None),
            request(false, None, Some(md5)),
            request(false, Some(md5), Some("1700000000")),
            request(false, None, Some("1700000000")),
            request(false, None, Some("1700000001")),
            request(false, None, Some("")),
        ];
        for (index, left) in all.iter().enumerate() {
            for right in &all[index + 1..] {
                assert_ne!(left, right);
            }
        }
        assert_eq!(
            request(true, Some(md5), None),
            request(true, Some(md5), None)
        );
    }

    #[test]
    fn projection_identity_uses_exact_spec_bytes() {
        let same = ProjectionId::new(b"{\"mount\":\"demo\"}\n");
        assert_eq!(same, ProjectionId::new(b"{\"mount\":\"demo\"}\n"));
        assert_ne!(same, ProjectionId::new(b"{\"mount\":\"demo\"}"));
    }
}
