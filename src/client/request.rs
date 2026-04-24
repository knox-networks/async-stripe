/// A minimal HTTP request type used internally to pass requests between
/// the stripe client and the transport backends (hyper, surf, etc.).
///
/// Using this instead of `http_types::Request` avoids pulling in `rand 0.7.x`
/// from the `http-types` crate as an unconditional dependency.
#[derive(Clone, Debug)]
pub(crate) struct StripeRequest {
    pub(crate) method: http::Method,
    pub(crate) url: url::Url,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl StripeRequest {
    pub(crate) fn new(method: http::Method, url: url::Url) -> Self {
        Self { method, url, headers: Vec::new(), body: Vec::new() }
    }

    pub(crate) fn insert_header(&mut self, key: &str, value: impl Into<String>) {
        self.headers.push((key.to_owned(), value.into()));
    }

    pub(crate) fn set_body(&mut self, body: Vec<u8>) {
        self.body = body;
    }
}
