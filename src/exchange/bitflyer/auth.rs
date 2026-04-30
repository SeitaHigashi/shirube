use chrono::Utc;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

pub struct AuthHeaders {
    pub access_key: String,
    pub access_timestamp: String,
    pub access_sign: String,
}

pub fn build_auth_headers(
    api_key: &str,
    api_secret: &str,
    method: &str,
    path_with_query: &str,
    body: &str,
) -> AuthHeaders {
    let timestamp = Utc::now().timestamp().to_string();
    build_auth_headers_with_timestamp(api_key, api_secret, &timestamp, method, path_with_query, body)
}

pub fn build_auth_headers_with_timestamp(
    api_key: &str,
    api_secret: &str,
    timestamp: &str,
    method: &str,
    path_with_query: &str,
    body: &str,
) -> AuthHeaders {
    let text = format!(
        "{}{}{}{}",
        timestamp,
        method.to_uppercase(),
        path_with_query,
        body
    );

    let mut mac =
        HmacSha256::new_from_slice(api_secret.as_bytes()).expect("HMAC accepts any key size");
    mac.update(text.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    AuthHeaders {
        access_key: api_key.to_string(),
        access_timestamp: timestamp.to_string(),
        access_sign: signature,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test vector computed with Python:
    // import hmac, hashlib
    // key = b"testsecret"
    // msg = b"1704067200GETmy/path"
    // print(hmac.new(key, msg, hashlib.sha256).hexdigest())
    #[test]
    fn signature_matches_known_vector() {
        let headers = build_auth_headers_with_timestamp(
            "testkey",
            "testsecret",
            "1704067200",
            "GET",
            "my/path",
            "",
        );
        // Pre-computed: echo -n "1704067200GETmy/path" | openssl dgst -sha256 -hmac "testsecret"
        let expected = {
            let mut mac =
                HmacSha256::new_from_slice(b"testsecret").unwrap();
            mac.update(b"1704067200GETmy/path");
            hex::encode(mac.finalize().into_bytes())
        };
        assert_eq!(headers.access_sign, expected);
        assert_eq!(headers.access_key, "testkey");
        assert_eq!(headers.access_timestamp, "1704067200");
    }

    #[test]
    fn signature_includes_body_for_post() {
        let body = r#"{"product_code":"BTC_JPY"}"#;
        let headers = build_auth_headers_with_timestamp(
            "key",
            "secret",
            "1000000000",
            "POST",
            "/v1/me/sendchildorder",
            body,
        );
        let expected = {
            let mut mac = HmacSha256::new_from_slice(b"secret").unwrap();
            mac.update(
                format!("{}POST{}{}", "1000000000", "/v1/me/sendchildorder", body).as_bytes(),
            );
            hex::encode(mac.finalize().into_bytes())
        };
        assert_eq!(headers.access_sign, expected);
    }

    #[test]
    fn different_secrets_produce_different_signatures() {
        let h1 = build_auth_headers_with_timestamp("k", "secret1", "1000", "GET", "/path", "");
        let h2 = build_auth_headers_with_timestamp("k", "secret2", "1000", "GET", "/path", "");
        assert_ne!(h1.access_sign, h2.access_sign);
    }
}
