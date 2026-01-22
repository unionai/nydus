// Copyright 2022 Ant Group. All rights reserved.
// Copyright (C) 2022 Alibaba Cloud. All rights reserved.

// SPDX-License-Identifier: Apache-2.0

// ! Storage backend driver to access blobs on s3.

use std::fmt::Debug;
use std::io::Result;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_config::provider_config::ProviderConfig;
use aws_config::Region;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings,
};
use aws_sigv4::sign::v4::SigningParams;
use aws_smithy_runtime_api::client::identity::Identity;

use nydus_api::S3Config;
use nydus_utils::metrics::BackendMetrics;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Method;
use tokio::runtime::Runtime;

use crate::backend::connection::{Connection, ConnectionConfig};
use crate::backend::object_storage::{ObjectStorage, ObjectStorageState};

const S3_DEFAULT_ENDPOINT: &str = "s3.amazonaws.com";

const EXPIRATION_SLACK: Duration = std::time::Duration::from_secs(10 * 60);

#[derive(Debug)]
pub struct S3State {
    region: String,
    scheme: String,
    object_prefix: String,
    endpoint: String,
    bucket_name: String,
    retry_limit: u8,
    identity: RefreshingIdentity,
    rt: Runtime,
}

/// Storage backend to access data stored in S3.
pub type S3 = ObjectStorage<S3State>;

impl S3 {
    /// Create a new S3 storage backend.
    pub fn new(s3_config: &S3Config, id: Option<&str>) -> Result<S3> {
        let con_config: ConnectionConfig = s3_config.clone().into();
        let retry_limit = con_config.retry_limit;
        let connection = Connection::new(&con_config)?;
        let final_endpoint = if s3_config.endpoint.is_empty() {
            S3_DEFAULT_ENDPOINT.to_string()
        } else {
            s3_config.endpoint.clone()
        };

        let rt = Runtime::new()?;
        // If explicit access credentials are provided, use those directly.
        // Otherwise, try to obtain credentials through the AWS credential chain
        // (environment variables, instance metadata, etc.)
        let credentials =
            if !s3_config.access_key_id.is_empty() && !s3_config.access_key_secret.is_empty() {
                static_credential_provider(
                    s3_config.access_key_id.clone(),
                    s3_config.access_key_secret.clone(),
                )
            } else {
                let region = Region::new(s3_config.region.clone());
                let provider_config = ProviderConfig::empty().with_region(Some(region));
                let provider = rt.block_on(
                    DefaultCredentialsChain::builder()
                        .configure(provider_config)
                        .build(),
                );
                SharedCredentialsProvider::new(provider)
            };

        let time_source: Arc<dyn TimeSource> = Arc::new(SystemTimeSource);
        let identity = rt.block_on(RefreshingIdentity::new(credentials, time_source))?;

        let state = Arc::new(S3State {
            region: s3_config.region.clone(),
            scheme: s3_config.scheme.clone(),
            object_prefix: s3_config.object_prefix.clone(),
            endpoint: final_endpoint,
            bucket_name: s3_config.bucket_name.clone(),
            retry_limit,
            identity,
            rt,
        });
        let metrics = id.map(|i| BackendMetrics::new(i, "oss"));

        Ok(ObjectStorage::new_object_storage(
            connection,
            state,
            metrics,
            id.map(|i| i.to_string()),
        ))
    }
}

impl ObjectStorageState for S3State {
    fn url(&self, obj_key: &str, query_str: &[&str]) -> (String, String) {
        let query_str = if query_str.is_empty() {
            "".to_string()
        } else {
            format!("?{}", query_str.join("&"))
        };
        let resource = format!(
            "/{}/{}{}{}",
            self.bucket_name, self.object_prefix, obj_key, query_str
        );
        let url = format!("{}://{}{}", self.scheme, self.endpoint, resource,);
        (resource, url)
    }

    // modified based on https://github.com/minio/minio-rs/blob/5fea81d68d381fd2a4c27e4d259f7012de08ab77/src/s3/signer.rs#L106-L135
    // under apache 2.0 license
    /// generate s3 request signature
    fn sign(
        &self,
        verb: Method,
        headers: &mut HeaderMap,
        _: &str,
        full_resource_url: &str,
    ) -> Result<()> {
        self.rt.block_on(self.identity.with_identity(|identity| {
            let mut signing_settings = SigningSettings::default();
            signing_settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;

            let signing_params = SigningParams::builder()
                .identity(&identity)
                .region(&self.region)
                .name("s3")
                .time(SystemTime::now())
                .settings(signing_settings)
                .build()
                .map_err(std::io::Error::other)?;

            let signable_request = SignableRequest::new(
                verb.as_str(),
                full_resource_url,
                headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), std::str::from_utf8(v.as_bytes()).unwrap())),
                SignableBody::empty(),
            )
            .map_err(std::io::Error::other)?;

            // Sign and then apply the signature to the request
            let (instructions, _) = aws_sigv4::http_request::sign(
                signable_request,
                &aws_sigv4::http_request::SigningParams::V4(signing_params),
            )
            .map_err(std::io::Error::other)?
            .into_parts();

            for (k, v) in instructions.headers() {
                let hn = HeaderName::from_str(k).map_err(std::io::Error::other)?;
                let hv = HeaderValue::from_str(v).map_err(std::io::Error::other)?;
                headers.insert(hn, hv);
            }

            Ok(())
        }))
    }

    fn retry_limit(&self) -> u8 {
        self.retry_limit
    }
}

fn static_credential_provider(
    access_key_id: String,
    access_key_secret: String,
) -> SharedCredentialsProvider {
    use aws_credential_types::credential_fn::provide_credentials_fn;

    let provider = provide_credentials_fn(move || {
        let access_key_id = access_key_id.clone();
        let access_key_secret = access_key_secret.clone();
        async move {
            Ok(Credentials::new(
                access_key_id,
                access_key_secret,
                None,
                None,
                "static",
            ))
        }
    });

    SharedCredentialsProvider::new(provider)
}

trait TimeSource: Send + Sync + Debug {
    fn now(&self) -> SystemTime;
}

#[derive(Debug)]
struct SystemTimeSource;

impl TimeSource for SystemTimeSource {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
#[derive(Debug)]
struct RefreshingIdentity {
    provider: SharedCredentialsProvider,
    time_source: Arc<dyn TimeSource>,
    creds: Mutex<Identity>,
}

impl RefreshingIdentity {
    async fn new(
        provider: SharedCredentialsProvider,
        time_source: Arc<dyn TimeSource>,
    ) -> Result<Self> {
        let creds = provider
            .provide_credentials()
            .await
            .map_err(std::io::Error::other)?;

        let expiry = creds.expiry();

        Ok(Self {
            provider,
            time_source,
            creds: Mutex::new(Identity::new(creds, expiry)),
        })
    }

    async fn with_identity<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(&Identity) -> Result<()>,
    {
        {
            let identity = self.creds.lock().unwrap();

            let need_refresh = {
                if let Some(expiry) = identity.expiration() {
                    self.time_source.now() >= expiry - EXPIRATION_SLACK
                } else {
                    false
                }
            };

            if !need_refresh {
                return f(&identity);
            }
        }

        let creds = self
            .provider
            .provide_credentials()
            .await
            .map_err(std::io::Error::other)?;

        let mut identity = self.creds.lock().unwrap();
        let expiry = creds.expiry();
        *identity = Identity::new(creds, expiry);

        f(&identity)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    use crate::backend::s3::{RefreshingIdentity, S3State, TimeSource};
    use crate::backend::BlobBackend;
    use crate::backend::{object_storage::ObjectStorageState, s3::static_credential_provider};
    use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
    use aws_credential_types::Credentials;
    use http::{HeaderMap, Method};
    use nydus_api::S3Config;
    use tokio::runtime::Runtime;

    use super::S3;

    #[derive(Debug)]
    struct MockTimeSource(Mutex<SystemTime>);

    impl MockTimeSource {
        fn new(t: SystemTime) -> Self {
            Self(Mutex::new(t))
        }

        fn advance(&self, dur: Duration) {
            *self.0.lock().unwrap() += dur
        }
    }

    impl TimeSource for MockTimeSource {
        fn now(&self) -> SystemTime {
            *self.0.lock().unwrap()
        }
    }

    fn get_test_s3_state() -> (S3State, String, String) {
        let rt = Runtime::new().unwrap();
        let credential_provider =
            static_credential_provider("test-key".to_string(), "test-key-secret".to_string());
        let time_source = Arc::new(MockTimeSource::new(SystemTime::now()));
        let identity = rt
            .block_on(RefreshingIdentity::new(credential_provider, time_source))
            .unwrap();
        let state = S3State {
            region: "us-east-1".to_string(),
            scheme: "http".to_string(),
            object_prefix: "test-prefix-".to_string(),
            endpoint: "localhost:9000".to_string(),
            bucket_name: "test-bucket".to_string(),
            retry_limit: 6,
            identity,
            rt,
        };
        let (resource, url) = state.url("test-object", &["a=b", "c=d"]);
        (state, resource, url)
    }

    #[test]
    fn test_s3_new() {
        let config_str = r#"{
            "endpoint": "https://test.com",
            "region": "us-east-1",
            "access_key_id": "test",
            "access_key_secret": "test",
            "bucket_name": "antsys-nydus",
            "object_prefix":"nydus_v2/",
            "retry_limit": 6
        }"#;
        let config: S3Config = serde_json::from_str(config_str).unwrap();
        let s3 = S3::new(&config, Some("test-image")).unwrap();

        s3.metrics();

        let reader = s3.get_reader("test").unwrap();
        assert_eq!(reader.retry_limit(), 6);

        s3.shutdown();
    }

    #[test]
    fn test_s3_state_url() {
        let (_, resource, url) = get_test_s3_state();
        assert_eq!(resource, "/test-bucket/test-prefix-test-object?a=b&c=d");
        assert_eq!(
            url,
            "http://localhost:9000/test-bucket/test-prefix-test-object?a=b&c=d"
        );
    }

    #[test]
    fn test_s3_state_sign() {
        let (state, resource, url) = get_test_s3_state();
        println!("{url}");
        let mut headers = HeaderMap::new();
        headers.append("Range", "bytes=5242900-".parse().unwrap());
        let result = state.sign(Method::GET, &mut headers, &resource, &url);
        assert!(result.is_ok());

        use regex::Regex;
        let re = Regex::new(r"^AWS4-HMAC-SHA256 Credential=test-key/[0-9]{8}/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=[A-Fa-f0-9]{64}$").unwrap();
        let authorization = headers.get("Authorization").unwrap();
        println!("{authorization:?}");
        assert!(re.is_match(authorization.to_str().unwrap()));
    }

    #[derive(Debug)]
    struct MockCredentialsProvider {
        time_source: Arc<dyn TimeSource>,
    }

    impl ProvideCredentials for MockCredentialsProvider {
        fn provide_credentials<'a>(
            &'a self,
        ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            let expiry = self.time_source.now() + Duration::from_secs(60 * 60);
            let creds = Credentials::new("test-key", "test-secret", None, Some(expiry), "mock");
            aws_credential_types::provider::future::ProvideCredentials::ready(Ok(creds))
        }
    }

    #[test]
    fn test_s3_refresh_credentials() {
        let time_source: Arc<MockTimeSource> = Arc::new(MockTimeSource::new(std::time::UNIX_EPOCH));
        let cred_provider = MockCredentialsProvider {
            time_source: time_source.clone(),
        };
        let rt = Runtime::new().unwrap();

        let identity = rt
            .block_on(RefreshingIdentity::new(
                SharedCredentialsProvider::new(cred_provider),
                time_source.clone(),
            ))
            .unwrap();

        // advance to 5 mins before expiration
        time_source.advance(Duration::from_secs(55 * 60));

        rt.block_on(identity.with_identity(|ident| {
            assert!(
                ident.expiration().unwrap() == time_source.now() + Duration::from_secs(60 * 60)
            );
            Ok(())
        }))
        .unwrap();
    }
}
