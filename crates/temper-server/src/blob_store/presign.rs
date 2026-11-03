//! Short-lived signed links to one object (SigV4 query authentication), so a reader that the server has already
//! authorized fetches the bytes from the object store itself, ranges and all, without the server carrying them.

use super::{BlobStore, BlobStoreBackend, S3BlobStore};
use crate::aws_sigv4;

/// The longest a link may live: SigV4 allows seven days; a link handed to a browser after an authorization check
/// should outlive that check by minutes, not days.
pub(crate) const MAX_PRESIGN_SECS: u64 = 60 * 60;

/// How the object store should label the bytes it sends for the link.
#[derive(Clone, Debug, Default)]
pub(crate) struct PresignedResponse<'a> {
    /// `response-content-type`: the stored object carries none of its own.
    pub content_type: Option<&'a str>,
    /// `response-content-disposition`, such as `attachment`.
    pub content_disposition: Option<&'a str>,
}

impl BlobStore {
    /// A `GET` link to `key` that works for `expires_secs` (at most [`MAX_PRESIGN_SECS`]). `None` for a local store,
    /// which has no address of its own, or a store without credentials to sign with.
    pub(crate) fn presigned_get_url(
        &self,
        key: &str,
        expires_secs: u64,
        response: &PresignedResponse<'_>,
    ) -> Option<String> {
        match &self.backend {
            BlobStoreBackend::LocalFs { .. } => None,
            BlobStoreBackend::S3(store) => {
                store.presigned_get_url(key, expires_secs, response, &aws_sigv4::amz_date_now())
            }
        }
    }
}

impl S3BlobStore {
    fn presigned_get_url(
        &self,
        key: &str,
        expires_secs: u64,
        response: &PresignedResponse<'_>,
        amz_date: &str,
    ) -> Option<String> {
        // A link carries the session token of the credentials that sign it, and temporary credentials can be rebuilt
        // from their token: a link is signed with read-only link credentials, or with the store's own only when they
        // are permanent (a link then gives away nothing but itself). Otherwise there is no link.
        let (access_key, secret_key, session_token) = match (&self.link, &self.session_token) {
            (Some(link), _) => (
                link.access_key.as_str(),
                link.secret_key.as_str(),
                link.session_token.as_deref(),
            ),
            (None, None) => (
                self.access_key.as_deref()?,
                self.secret_key.as_deref()?,
                None,
            ),
            (None, Some(_)) => return None,
        };
        assert!(amz_date.len() >= 8, "amz_date is %Y%m%dT%H%M%SZ");
        let expires = expires_secs.clamp(1, MAX_PRESIGN_SECS).to_string();
        let url = self.object_url(key);
        let (host, path) = aws_sigv4::parse_url_host_path(&url);
        let date = &amz_date[..8];
        let scope = format!("{date}/auto/s3/aws4_request");
        let credential = format!("{access_key}/{scope}");
        let mut query: Vec<(&str, &str)> = vec![
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256"),
            ("X-Amz-Credential", &credential),
            ("X-Amz-Date", amz_date),
            ("X-Amz-Expires", &expires),
            ("X-Amz-SignedHeaders", "host"),
        ];
        if let Some(token) = session_token {
            query.push(("X-Amz-Security-Token", token));
        }
        if let Some(content_type) = response.content_type.filter(|v| !v.is_empty()) {
            query.push(("response-content-type", content_type));
        }
        if let Some(disposition) = response.content_disposition.filter(|v| !v.is_empty()) {
            query.push(("response-content-disposition", disposition));
        }
        query.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let canonical_query = query
            .iter()
            .map(|(name, value)| format!("{}={}", query_encode(name), query_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let canonical_uri = aws_sigv4::uri_encode_path(path);
        let canonical_request = format!(
            "GET\n{canonical_uri}\n{canonical_query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD"
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            aws_sigv4::sha256_hex(canonical_request.as_bytes())
        );
        let signing_key = aws_sigv4::derive_signing_key(secret_key, date, "auto", "s3");
        let signature = aws_sigv4::hex_encode(&aws_sigv4::hmac_sha256(
            &signing_key,
            string_to_sign.as_bytes(),
        ));
        let scheme_end = url.find("://").map_or(0, |at| at + 3);
        Some(format!(
            "{}{host}{canonical_uri}?{canonical_query}&X-Amz-Signature={signature}",
            &url[..scheme_end]
        ))
    }
}

/// Percent-encode a query name or value per SigV4: everything but unreserved characters, `/` included.
fn query_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 16);
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => {
                out.push('%');
                out.push(b"0123456789ABCDEF"[(byte >> 4) as usize] as char);
                out.push(b"0123456789ABCDEF"[(byte & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(token: Option<&str>) -> BlobStore {
        BlobStore::s3(
            "https://acct.r2.cloudflarestorage.com",
            "bucket",
            Some("AKID".to_string()),
            Some("secret".to_string()),
            "/tmp",
            Some("people/t1/a1".to_string()),
        )
        .with_session_token(token.map(str::to_string))
    }

    fn link(token: &str) -> Option<super::super::LinkCredentials> {
        Some(super::super::LinkCredentials {
            access_key: "LINKID".to_string(),
            secret_key: "link-secret".to_string(),
            session_token: Some(token.to_string()),
        })
    }

    fn signed(store: &BlobStore, response: &PresignedResponse<'_>) -> String {
        let BlobStoreBackend::S3(s3) = &store.backend else {
            panic!("an S3 store");
        };
        s3.presigned_get_url("temper-fs/sha256:ab", 120, response, "20261015T120000Z")
            .expect("credentials sign")
    }

    #[test]
    fn a_link_names_the_prefixed_key_its_lifetime_and_the_link_credentials_session_token() {
        let url = signed(
            &store(Some("write-token")).with_link_credentials(link("tok/en=")),
            &PresignedResponse {
                content_type: Some("image/png"),
                content_disposition: None,
            },
        );
        assert!(url.starts_with(
            "https://acct.r2.cloudflarestorage.com/bucket/people/t1/a1/temper-fs/sha256%3Aab?"
        ));
        assert!(url.contains("X-Amz-Expires=120&"));
        assert!(url.contains("X-Amz-Security-Token=tok%2Fen%3D&"));
        assert!(
            url.contains("X-Amz-Credential=LINKID%2F"),
            "signed with the link credentials"
        );
        assert!(
            !url.contains("write-token") && !url.contains("AKID"),
            "the store's own never go into a link"
        );
        assert!(url.contains("response-content-type=image%2Fpng"));
        assert!(url.contains("&X-Amz-Signature="));
        // The query is in canonical (sorted) order, so what was signed is what is sent.
        let names: Vec<&str> = url
            .split('?')
            .nth(1)
            .unwrap_or_default()
            .split('&')
            .map(|pair| pair.split('=').next().unwrap_or_default())
            .collect();
        let mut sorted = names[..names.len() - 1].to_vec();
        sorted.sort_unstable();
        assert_eq!(names[..names.len() - 1], sorted[..]);
    }

    #[test]
    fn a_link_without_a_token_or_a_label_leaves_them_out_and_a_local_store_has_none() {
        let url = signed(&store(None), &PresignedResponse::default());
        assert!(!url.contains("X-Amz-Security-Token"));
        assert!(!url.contains("response-content"));
        assert!(
            BlobStore::local_fs("/tmp/x")
                .presigned_get_url("k", 60, &PresignedResponse::default())
                .is_none()
        );
    }

    #[test]
    fn temporary_store_credentials_without_link_credentials_sign_no_link() {
        let BlobStoreBackend::S3(s3) = &store(Some("write-token")).backend else {
            panic!("an S3 store");
        };
        assert!(
            s3.presigned_get_url("k", 60, &PresignedResponse::default(), "20261015T120000Z")
                .is_none(),
            "a link would carry the token, and the token rebuilds the credentials that write"
        );
    }

    #[test]
    fn a_link_never_lives_longer_than_an_hour() {
        let BlobStoreBackend::S3(s3) = &store(None).backend else {
            panic!("an S3 store");
        };
        let url = s3
            .presigned_get_url(
                "k",
                7 * 24 * 3600,
                &PresignedResponse::default(),
                "20261015T120000Z",
            )
            .expect("signed");
        assert!(url.contains("X-Amz-Expires=3600&"));
    }
}
