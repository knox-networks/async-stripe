use std::future::{self, Future};
use std::pin::Pin;

use http_body_util::{BodyExt, Full};
use http_types::{Request, StatusCode};
use hyper::body::Bytes;
use hyper_util::client::legacy::connect::HttpConnector;
use serde::de::DeserializeOwned;
use tokio::time::sleep;

use crate::client::request_strategy::{Outcome, RequestStrategy};
use crate::error::{ErrorResponse, StripeError};

#[cfg(feature = "hyper-rustls-native")]
mod connector {
    use hyper_rustls::HttpsConnectorBuilder;
    use hyper_util::client::legacy::connect::HttpConnector;
    pub use hyper_rustls::HttpsConnector;

    pub fn create() -> HttpsConnector<HttpConnector> {
        HttpsConnectorBuilder::new()
            .with_native_roots()
            .expect("Failed to load native TLS certificates")
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build()
    }
}

#[cfg(feature = "hyper-rustls-webpki")]
mod connector {
    use hyper_rustls::HttpsConnectorBuilder;
    use hyper_util::client::legacy::connect::HttpConnector;
    pub use hyper_rustls::HttpsConnector;

    pub fn create() -> HttpsConnector<HttpConnector> {
        HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .build()
    }
}

#[cfg(feature = "hyper-tls")]
mod connector {
    use hyper_util::client::legacy::connect::HttpConnector;
    pub use hyper_tls::HttpsConnector;

    pub fn create() -> HttpsConnector<HttpConnector> {
        HttpsConnector::new()
    }
}

#[cfg(all(feature = "hyper-tls", feature = "hyper-rustls"))]
compile_error!("You must enable only one TLS implementation");

type HttpClient =
    hyper_util::client::legacy::Client<connector::HttpsConnector<HttpConnector>, Full<Bytes>>;

pub type Response<T> = Pin<Box<dyn Future<Output = Result<T, StripeError>> + Send>>;

#[allow(dead_code)]
#[inline(always)]
pub(crate) fn ok<T: Send + 'static>(ok: T) -> Response<T> {
    Box::pin(future::ready(Ok(ok)))
}

#[allow(dead_code)]
#[inline(always)]
pub(crate) fn err<T: Send + 'static>(err: StripeError) -> Response<T> {
    Box::pin(future::ready(Err(err)))
}

#[derive(Clone)]
pub struct TokioClient {
    client: HttpClient,
}

impl Default for TokioClient {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioClient {
    pub fn new() -> Self {
        Self {
            client: hyper_util::client::legacy::Client::builder(
                hyper_util::rt::TokioExecutor::new(),
            )
            .pool_max_idle_per_host(0)
            .build(connector::create()),
        }
    }

    pub fn execute<T: DeserializeOwned + Send + 'static>(
        &self,
        request: Request,
        strategy: &RequestStrategy,
    ) -> Response<T> {
        // need to clone here since client could be used across threads.
        // N.B. Client is send sync; cloned clients share the same pool.
        let client = self.client.clone();
        let strategy = strategy.clone();

        Box::pin(async move {
            let bytes = send_inner(&client, request, &strategy).await?;
            let json_deserializer = &mut serde_json::Deserializer::from_slice(&bytes);
            serde_path_to_error::deserialize(json_deserializer).map_err(StripeError::from)
        })
    }
}

async fn send_inner(
    client: &HttpClient,
    mut request: Request,
    strategy: &RequestStrategy,
) -> Result<Bytes, StripeError> {
    let mut tries = 0;
    let mut last_status: Option<StatusCode> = None;
    let mut last_retry_header: Option<bool> = None;

    // if we have no last error, then the strategy is invalid
    let mut last_error = StripeError::ClientError("Invalid strategy".to_string());

    if let Some(key) = strategy.get_key() {
        request.insert_header("Idempotency-Key", key);
    }

    let body = request.body_bytes().await?;

    loop {
        return match strategy.test(last_status, last_retry_header, tries) {
            Outcome::Stop => Err(last_error),
            Outcome::Continue(duration) => {
                if let Some(duration) = duration {
                    sleep(duration).await;
                }

                // note: hyper::Request provides no easy way to clone, so we perform
                //       the conversion from the clonable http_types::Request each time
                //       obviously cloning before the first request is not ideal
                let mut request = request.clone();
                request.set_body(body.clone());

                let response = match client.request(convert_request(request).await).await {
                    Ok(response) => response,
                    Err(err) => {
                        last_error = StripeError::ClientError(err.to_string());
                        tries += 1;
                        continue;
                    }
                };

                let status = response.status();
                let retry = response
                    .headers()
                    .get("Stripe-Should-Retry")
                    .and_then(|s| s.to_str().ok())
                    .and_then(|s| s.parse().ok());

                let bytes = response.into_body().collect().await?.to_bytes();

                if !status.is_success() {
                    tries += 1;
                    let json_deserializer = &mut serde_json::Deserializer::from_slice(&bytes);
                    last_error = serde_path_to_error::deserialize(json_deserializer)
                        .map(|mut e: ErrorResponse| {
                            e.error.http_status = status.into();
                            StripeError::from(e.error)
                        })
                        .unwrap_or_else(StripeError::from);
                    last_status = Some(
                        // NOTE: StatusCode::try_from can fail here, so fall back to InternalServerError
                        StatusCode::try_from(u16::from(status))
                            .unwrap_or(StatusCode::InternalServerError),
                    );
                    last_retry_header = retry;
                    continue;
                }

                Ok(bytes)
            }
        };
    }
}

/// convert an http_types::Request with an http_types::Body into a hyper::Request<Full<Bytes>>
///
/// note: this is necessary because hyper 1.x uses `http` 1.x types while http_types only
///       provides conversion to `http` 0.2.x, so we convert the fields manually.
async fn convert_request(mut request: http_types::Request) -> hyper::Request<Full<Bytes>> {
    let body = request.body_bytes().await.expect("We know the data is a valid bytes object.");
    let url = request.url().as_str().to_string();
    let method = request.method().as_ref().to_string();
    let mut builder = hyper::Request::builder().method(method.as_str()).uri(url.as_str());
    for (name, values) in request.iter() {
        for value in values.iter() {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }
    builder.body(Full::new(Bytes::from(body))).expect("valid request parts")
}

#[cfg(test)]
mod tests {
    use http_body_util::{BodyExt, Full};
    use http_types::{Method, Request, Url};
    use hyper::body::Bytes;
    use hyper::Request as HyperRequest;
    use httpmock::prelude::*;

    use super::convert_request;
    use super::TokioClient;
    use crate::client::request_strategy::RequestStrategy;
    use crate::StripeError;

    const TEST_URL: &str = "https://api.stripe.com/v1/";

    #[tokio::test]
    async fn basic_conversion() {
        req_equal(
            convert_request(Request::new(Method::Get, TEST_URL)).await,
            HyperRequest::builder()
                .method("GET")
                .uri("http://test.com")
                .body(Full::new(Bytes::new()))
                .unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn bytes_body_conversion() {
        let body = "test".as_bytes();

        let mut req = Request::new(Method::Post, TEST_URL);
        req.set_body(body);

        req_equal(
            convert_request(req).await,
            HyperRequest::builder()
                .method("POST")
                .uri(TEST_URL)
                .body(Full::new(Bytes::from(body)))
                .unwrap(),
        )
        .await;
    }

    #[tokio::test]
    async fn string_body_conversion() {
        let body = "test";

        let mut req = Request::new(Method::Post, TEST_URL);
        req.set_body(body);

        req_equal(
            convert_request(req).await,
            HyperRequest::builder()
                .method("POST")
                .uri(TEST_URL)
                .body(Full::new(Bytes::from(body)))
                .unwrap(),
        )
        .await;
    }

    async fn req_equal(a: HyperRequest<Full<Bytes>>, b: HyperRequest<Full<Bytes>>) {
        let (a_parts, a_body) = a.into_parts();
        let (b_parts, b_body) = b.into_parts();

        assert_eq!(a_parts.method, b_parts.method);
        assert_eq!(
            a_body.collect().await.unwrap().to_bytes().len(),
            b_body.collect().await.unwrap().to_bytes().len()
        );
    }

    #[tokio::test]
    async fn retry() {
        let client = TokioClient::new();

        // Start a lightweight mock server.
        let server = MockServer::start_async().await;

        // Create a mock on the server.
        let hello_mock = server.mock(|when, then| {
            when.method(GET).path("/server-errors");
            then.status(500);
        });

        let req = Request::get(Url::parse(&server.url("/server-errors")).unwrap());
        let res = client.execute::<()>(req, &RequestStrategy::Retry(5)).await;

        hello_mock.assert_hits_async(5).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn user_error() {
        let client = TokioClient::new();

        // Start a lightweight mock server.
        let server = MockServer::start_async().await;

        let mock = server.mock(|when, then| {
            when.method(GET).path("/v1/missing");
            then.status(404).body("{
                \"error\": {
                  \"message\": \"Unrecognized request URL (GET: /v1/missing). Please see https://stripe.com/docs or we can help at https://support.stripe.com/.\",
                  \"type\": \"invalid_request_error\"
                }
              }
              ");
        });

        let req = Request::get(Url::parse(&server.url("/v1/missing")).unwrap());
        let res = client.execute::<()>(req, &RequestStrategy::Retry(3)).await;

        mock.assert_hits_async(1).await;

        match res {
            Err(StripeError::Stripe(x)) => println!("{:?}", x),
            _ => panic!("Expected stripe error {:?}", res),
        }
    }

    #[tokio::test]
    async fn nice_serde_error() {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        struct DataType {
            // Allowing dead code since used for deserialization
            #[allow(dead_code)]
            id: String,
            #[allow(dead_code)]
            name: String,
        }

        let client = TokioClient::new();

        // Start a lightweight mock server.
        let server = MockServer::start_async().await;

        let mock = server.mock(|when, then| {
            when.method(GET).path("/v1/odd_data");
            then.status(200).body(
                "{
                \"id\": \"test\",
                \"name\": 10
              }
              ",
            );
        });

        let req = Request::get(Url::parse(&server.url("/v1/odd_data")).unwrap());
        let res = client.execute::<DataType>(req, &RequestStrategy::Retry(3)).await;

        mock.assert_hits_async(1).await;

        match res {
            Err(StripeError::JSONSerialize(err)) => {
                println!("Error: {:?} Path: {:?}", err.inner(), err.path().to_string())
            }
            _ => panic!("Expected stripe error {:?}", res),
        }
    }

    #[tokio::test]
    async fn retry_header() {
        let client = TokioClient::new();

        // Start a lightweight mock server.
        let server = MockServer::start_async().await;

        // Create a mock on the server.
        let hello_mock = server.mock(|when, then| {
            when.method(GET).path("/server-errors");
            then.status(500).header("Stripe-Should-Retry", "false");
        });

        let req = Request::get(Url::parse(&server.url("/server-errors")).unwrap());
        let res = client.execute::<()>(req, &RequestStrategy::Retry(5)).await;

        hello_mock.assert_hits_async(1).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn retry_body() {
        let client = TokioClient::new();

        // Start a lightweight mock server.
        let server = MockServer::start_async().await;

        // Create a mock on the server.
        let hello_mock = server.mock(|when, then| {
            when.method(POST).path("/server-errors").body("body");
            then.status(500);
        });

        let mut req = Request::post(Url::parse(&server.url("/server-errors")).unwrap());
        req.set_body("body");
        let res = client.execute::<()>(req, &RequestStrategy::Retry(5)).await;

        hello_mock.assert_hits_async(5).await;
        assert!(res.is_err());
    }
}
