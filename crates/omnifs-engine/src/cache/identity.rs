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
    /// Hash validated request material plus the stable credential partition,
    /// the MD5 of the bytes that the provider asks for, as the request of
    /// each content without one, and the version of the bytes of a request
    /// that is the same for each version, as a part that no MD5 gives, with
    /// nothing for a request without one, which keeps its identity. Token
    /// bytes and refresh generations never enter this identity.
    pub(crate) fn new(
        auth: Option<&CredentialId>,
        method: &str,
        canonical_url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        md5: Option<&str>,
        version: Option<&str>,
    ) -> Self {
        let mut body_material = Vec::new();
        match body {
            Some(bytes) => {
                body_material.push(1);
                frame_bytes(&mut body_material, bytes);
            },
            None => body_material.push(0),
        }
        let auth_partition = auth.map(ToString::to_string);
        let headers = encode_headers(headers);
        // An MD5 has lowercase hexadecimal digits alone.
        let version = version.map(|version| [b"version:".as_slice(), version.as_bytes()].concat());
        let mut parts = vec![
            auth_partition
                .as_deref()
                .unwrap_or("unauthenticated")
                .as_bytes(),
            method.as_bytes(),
            canonical_url.as_bytes(),
            &headers,
            &body_material,
        ];
        parts.extend(md5.map(str::as_bytes));
        parts.extend(version.as_deref());
        Self(hash_parts(b"omnifs/blob-request/v1", &parts))
    }

    pub(crate) fn filesystem_name(self) -> String {
        hex(self.0)
    }
}

impl GitId {
    pub(crate) fn new(mount_scope: &str, canonical_remote: &str, reference: Option<&str>) -> Self {
        let mut reference_material = Vec::new();
        match reference {
            Some(value) => {
                reference_material.push(1);
                frame_bytes(&mut reference_material, value.as_bytes());
            },
            None => reference_material.push(0),
        }
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

    #[test]
    fn identities_are_unambiguous_and_mount_scoped() {
        assert_ne!(
            BlobRequestId::new(None, "GET", "https://example.test/A", &[], None, None, None),
            BlobRequestId::new(None, "GET", "https://example.test/a", &[], None, None, None),
        );
        assert_ne!(
            BlobRequestId::new(None, "POST", "https://example.test", &[], None, None, None),
            BlobRequestId::new(
                None,
                "POST",
                "https://example.test",
                &[],
                Some(b""),
                None,
                None,
            ),
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
        let same_request = |auth| {
            BlobRequestId::new(
                auth,
                "GET",
                "https://api.example.test/data",
                &[],
                None,
                None,
                None,
            )
        };

        assert_eq!(same_request(Some(&first)), same_request(Some(&first)));
        assert_ne!(same_request(Some(&first)), same_request(Some(&second)));
        assert_ne!(same_request(None), same_request(Some(&first)));
    }

    /// The version of a request is a part of its identity that no MD5
    /// gives: each version is the identity of another request.
    #[test]
    fn a_version_is_a_part_of_the_identity_that_no_md5_gives() {
        let request = |md5, version| {
            BlobRequestId::new(
                None,
                "GET",
                "https://files.example.test/file",
                &[],
                None,
                md5,
                version,
            )
        };

        assert_ne!(request(None, Some("1700000000")), request(None, None));
        assert_ne!(
            request(None, Some("1700000000")),
            request(None, Some("1700000001"))
        );
        assert_ne!(
            request(None, Some("3d93da86c9045cb1b17e96790cd19789")),
            request(Some("3d93da86c9045cb1b17e96790cd19789"), None)
        );
        assert_ne!(
            request(Some("3d93da86c9045cb1b17e96790cd19789"), Some("1700000000")),
            request(Some("3d93da86c9045cb1b17e96790cd19789"), None)
        );
    }

    /// A request without a version has the identity of its parts alone, as
    /// before the version.
    #[test]
    fn a_request_without_a_version_keeps_its_identity() {
        let url = "https://files.example.test/file";
        let md5 = "3d93da86c9045cb1b17e96790cd19789";
        let parts: [&[u8]; 5] = [b"unauthenticated", b"GET", url.as_bytes(), b"", &[0]];
        assert_eq!(
            BlobRequestId::new(None, "GET", url, &[], None, None, None),
            BlobRequestId(hash_parts(b"omnifs/blob-request/v1", &parts)),
        );
        let parts: [&[u8]; 6] = [
            b"unauthenticated",
            b"GET",
            url.as_bytes(),
            b"",
            &[0],
            md5.as_bytes(),
        ];
        assert_eq!(
            BlobRequestId::new(None, "GET", url, &[], None, Some(md5), None),
            BlobRequestId(hash_parts(b"omnifs/blob-request/v1", &parts)),
        );
    }

    #[test]
    fn projection_identity_uses_exact_spec_bytes() {
        let same = ProjectionId::new(b"{\"mount\":\"demo\"}\n");
        assert_eq!(same, ProjectionId::new(b"{\"mount\":\"demo\"}\n"));
        assert_ne!(same, ProjectionId::new(b"{\"mount\":\"demo\"}"));
    }
}
