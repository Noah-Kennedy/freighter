extern crate base64;
extern crate md5;

use super::request_trait::ResponseData;
use crate::bucket::Bucket;
use crate::command::Command;
use crate::command::HttpMethod;
use crate::error::S3Error;
use base64::engine::general_purpose;
use base64::Engine;
use bytes::Bytes;
use futures::TryStreamExt;
use hmac::Mac;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, DATE, HOST, RANGE};
use http::{HeaderMap, HeaderName};
use hyper::Body;
use std::collections::HashMap;
use std::fmt::Write;
use time::format_description::well_known::Rfc2822;
use time::OffsetDateTime;

use crate::{signing, LONG_DATETIME};
use tokio_stream::StreamExt;
use url::Url;

// Temporary structure for making a request
pub struct HyperRequest<'a> {
    pub bucket: &'a Bucket,
    pub path: &'a str,
    pub command: Command<'a>,
    pub datetime: OffsetDateTime,
    pub sync: bool,
}

impl<'a> HyperRequest<'a> {
    async fn response(&self) -> Result<reqwest::Response, S3Error> {
        // Build headers
        let headers = match self.headers() {
            Ok(headers) => headers,
            Err(e) => return Err(e),
        };

        let client = reqwest::Client::new();

        let method = match self.command.http_verb() {
            HttpMethod::Delete => http::Method::DELETE,
            HttpMethod::Get => http::Method::GET,
            HttpMethod::Post => http::Method::POST,
            HttpMethod::Put => http::Method::PUT,
            HttpMethod::Head => http::Method::HEAD,
        };

        let request = {
            let mut request = http::Request::builder()
                .method(method)
                .uri(self.url()?.as_str());

            for (header, value) in headers.iter() {
                request = request.header(header, value);
            }

            request.body(Body::from(self.request_body()))?
        };
        let response = client.execute(request.try_into().unwrap()).await?;

        if cfg!(feature = "fail-on-err") && !response.status().is_success() {
            let status = response.status().as_u16();
            let text = String::from_utf8(response.bytes().await?.into())?;
            return Err(S3Error::HttpFailWithBody(status, text));
        }

        Ok(response)
    }

    pub async fn response_data(&self, etag: bool) -> Result<ResponseData, S3Error> {
        let response = self.response().await?;
        let status_code = response.status().as_u16();
        let mut headers = response.headers().clone();
        let response_headers = headers
            .clone()
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.to_str()
                        .unwrap_or("could-not-decode-header-value")
                        .to_string(),
                )
            })
            .collect::<HashMap<String, String>>();
        let body_vec = if etag {
            if let Some(etag) = headers.remove("ETag") {
                Bytes::from(etag.to_str()?.to_string())
            } else {
                Bytes::from("")
            }
        } else {
            response.bytes().await?
        };
        Ok(ResponseData::new(body_vec, status_code, response_headers))
    }

    pub async fn response_data_to_writer<T: tokio::io::AsyncWrite + Send + Unpin>(
        &self,
        writer: &mut T,
    ) -> Result<u16, S3Error> {
        use tokio::io::AsyncWriteExt;
        let response = self.response().await?;

        let status_code = response.status();
        let mut stream = response.bytes_stream().into_stream();

        while let Some(item) = stream.next().await {
            writer.write_all(&item?).await?;
        }

        Ok(status_code.as_u16())
    }

    pub async fn response_header(&self) -> Result<(HeaderMap, u16), S3Error> {
        let response = self.response().await?;
        let status_code = response.status().as_u16();
        let headers = response.headers().clone();
        Ok((headers, status_code))
    }

    fn datetime(&self) -> OffsetDateTime {
        self.datetime
    }

    fn bucket(&self) -> Bucket {
        self.bucket.clone()
    }

    fn command(&self) -> Command {
        self.command.clone()
    }

    fn path(&self) -> String {
        self.path.to_string()
    }

    fn signing_key(&self) -> Result<Vec<u8>, S3Error> {
        signing::signing_key(
            &self.datetime(),
            &self
                .bucket()
                .secret_key()?
                .expect("Secret key must be provided to sign headers, found None"),
            &self.bucket().region(),
            "s3",
        )
    }

    fn request_body(&self) -> Vec<u8> {
        if let Command::PutObject { content, .. } = self.command() {
            Vec::from(content)
        } else if let Command::PutObjectTagging { tags } = self.command() {
            Vec::from(tags)
        } else if let Command::UploadPart { content, .. } = self.command() {
            Vec::from(content)
        } else if let Command::CompleteMultipartUpload { data, .. } = &self.command() {
            let body = data.to_string();
            println!("CompleteMultipartUpload: {}", body);
            body.as_bytes().to_vec()
        } else if let Command::CreateBucket { config } = &self.command() {
            if let Some(payload) = config.location_constraint_payload() {
                Vec::from(payload)
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    fn long_date(&self) -> Result<String, S3Error> {
        Ok(self.datetime().format(LONG_DATETIME)?)
    }

    fn string_to_sign(&self, request: &str) -> Result<String, S3Error> {
        match self.command() {
            Command::PresignPost { post_policy, .. } => Ok(post_policy),
            _ => Ok(signing::string_to_sign(
                &self.datetime(),
                &self.bucket().region(),
                request,
            )?),
        }
    }

    fn host_header(&self) -> String {
        self.bucket().host()
    }

    pub(crate) fn presigned(&self) -> Result<String, S3Error> {
        let (expiry, custom_headers, custom_queries) = match self.command() {
            Command::PresignGet {
                expiry_secs,
                custom_queries,
            } => (expiry_secs, None, custom_queries),
            Command::PresignPut {
                expiry_secs,
                custom_headers,
            } => (expiry_secs, custom_headers, None),
            Command::PresignDelete { expiry_secs } => (expiry_secs, None, None),
            _ => unreachable!(),
        };

        Ok(format!(
            "{}&X-Amz-Signature={}",
            self.presigned_url_no_sig(expiry, custom_headers.as_ref(), custom_queries.as_ref())?,
            self.presigned_authorization(custom_headers.as_ref())?
        ))
    }

    fn presigned_authorization(
        &self,
        custom_headers: Option<&HeaderMap>,
    ) -> Result<String, S3Error> {
        let mut headers = HeaderMap::new();
        let host_header = self.host_header();
        headers.insert(HOST, host_header.parse()?);
        if let Some(custom_headers) = custom_headers {
            for (k, v) in custom_headers.iter() {
                headers.insert(k.clone(), v.clone());
            }
        }
        let canonical_request = self.presigned_canonical_request(&headers)?;
        let string_to_sign = self.string_to_sign(&canonical_request)?;
        let mut hmac = signing::HmacSha256::new_from_slice(&self.signing_key()?)?;
        hmac.update(string_to_sign.as_bytes());
        let signature = hex::encode(hmac.finalize().into_bytes());
        Ok(signature)
    }

    fn presigned_canonical_request(&self, headers: &HeaderMap) -> Result<String, S3Error> {
        let (expiry, custom_headers, custom_queries) = match self.command() {
            Command::PresignGet {
                expiry_secs,
                custom_queries,
            } => (expiry_secs, None, custom_queries),
            Command::PresignPut {
                expiry_secs,
                custom_headers,
            } => (expiry_secs, custom_headers, None),
            Command::PresignDelete { expiry_secs } => (expiry_secs, None, None),
            _ => unreachable!(),
        };

        signing::canonical_request(
            &self.command().http_verb().to_string(),
            &self.presigned_url_no_sig(expiry, custom_headers.as_ref(), custom_queries.as_ref())?,
            headers,
            "UNSIGNED-PAYLOAD",
        )
    }

    fn presigned_url_no_sig(
        &self,
        expiry: u32,
        custom_headers: Option<&HeaderMap>,
        custom_queries: Option<&HashMap<String, String>>,
    ) -> Result<Url, S3Error> {
        let bucket = self.bucket();
        let token = if let Some(security_token) = bucket.security_token()? {
            Some(security_token)
        } else {
            bucket.session_token()?
        };
        let url = Url::parse(&format!(
            "{}{}{}",
            self.url()?,
            &signing::authorization_query_params_no_sig(
                &self.bucket().access_key()?.unwrap_or_default(),
                &self.datetime(),
                &self.bucket().region(),
                expiry,
                custom_headers,
                token.as_ref()
            )?,
            &signing::flatten_queries(custom_queries)?,
        ))?;

        Ok(url)
    }

    fn url(&self) -> Result<Url, S3Error> {
        let mut url_str = self.bucket().url();

        if let Command::CreateBucket { .. } = self.command() {
            return Ok(Url::parse(&url_str)?);
        }

        let path = if self.path().starts_with('/') {
            self.path()[1..].to_string()
        } else {
            self.path()[..].to_string()
        };

        url_str.push('/');
        url_str.push_str(&signing::uri_encode(&path, false));

        // Append to url_path
        #[allow(clippy::collapsible_match)]
        match self.command() {
            Command::InitiateMultipartUpload { .. } | Command::ListMultipartUploads { .. } => {
                url_str.push_str("?uploads")
            }
            Command::AbortMultipartUpload { upload_id } => {
                write!(url_str, "?uploadId={}", upload_id).expect("Could not write to url_str");
            }
            Command::CompleteMultipartUpload { upload_id, .. } => {
                write!(url_str, "?uploadId={}", upload_id).expect("Could not write to url_str");
            }
            Command::GetObjectTorrent => url_str.push_str("?torrent"),
            Command::PutObject { multipart, .. } => {
                if let Some(multipart) = multipart {
                    url_str.push_str(&multipart.query_string())
                }
            }
            _ => {}
        }

        let mut url = Url::parse(&url_str)?;

        for (key, value) in &self.bucket().extra_query {
            url.query_pairs_mut().append_pair(key, value);
        }

        if let Command::ListObjectsV2 {
            prefix,
            delimiter,
            continuation_token,
            start_after,
            max_keys,
        } = self.command().clone()
        {
            let mut query_pairs = url.query_pairs_mut();
            delimiter.map(|d| query_pairs.append_pair("delimiter", &d));

            query_pairs.append_pair("prefix", &prefix);
            query_pairs.append_pair("list-type", "2");
            if let Some(token) = continuation_token {
                query_pairs.append_pair("continuation-token", &token);
            }
            if let Some(start_after) = start_after {
                query_pairs.append_pair("start-after", &start_after);
            }
            if let Some(max_keys) = max_keys {
                query_pairs.append_pair("max-keys", &max_keys.to_string());
            }
        }

        if let Command::ListObjects {
            prefix,
            delimiter,
            marker,
            max_keys,
        } = self.command().clone()
        {
            let mut query_pairs = url.query_pairs_mut();
            delimiter.map(|d| query_pairs.append_pair("delimiter", &d));

            query_pairs.append_pair("prefix", &prefix);
            if let Some(marker) = marker {
                query_pairs.append_pair("marker", &marker);
            }
            if let Some(max_keys) = max_keys {
                query_pairs.append_pair("max-keys", &max_keys.to_string());
            }
        }

        match self.command() {
            Command::ListMultipartUploads {
                prefix,
                delimiter,
                key_marker,
                max_uploads,
            } => {
                let mut query_pairs = url.query_pairs_mut();
                delimiter.map(|d| query_pairs.append_pair("delimiter", d));
                if let Some(prefix) = prefix {
                    query_pairs.append_pair("prefix", prefix);
                }
                if let Some(key_marker) = key_marker {
                    query_pairs.append_pair("key-marker", &key_marker);
                }
                if let Some(max_uploads) = max_uploads {
                    query_pairs.append_pair("max-uploads", max_uploads.to_string().as_str());
                }
            }
            Command::PutObjectTagging { .. }
            | Command::GetObjectTagging
            | Command::DeleteObjectTagging => {
                url.query_pairs_mut().append_pair("tagging", "");
            }
            _ => {}
        }

        Ok(url)
    }

    fn canonical_request(&self, headers: &HeaderMap) -> Result<String, S3Error> {
        signing::canonical_request(
            &self.command().http_verb().to_string(),
            &self.url()?,
            headers,
            &self.command().sha256(),
        )
    }

    fn authorization(&self, headers: &HeaderMap) -> Result<String, S3Error> {
        let canonical_request = self.canonical_request(headers)?;
        let string_to_sign = self.string_to_sign(&canonical_request)?;
        let mut hmac = signing::HmacSha256::new_from_slice(&self.signing_key()?)?;
        hmac.update(string_to_sign.as_bytes());
        let signature = hex::encode(hmac.finalize().into_bytes());
        let signed_header = signing::signed_header_string(headers);
        signing::authorization_header(
            &self.bucket().access_key()?.expect("No access_key provided"),
            &self.datetime(),
            &self.bucket().region(),
            &signed_header,
            &signature,
        )
    }

    fn headers(&self) -> Result<HeaderMap, S3Error> {
        // Generate this once, but it's used in more than one place.
        let sha256 = self.command().sha256();

        // Start with extra_headers, that way our headers replace anything with
        // the same name.

        let mut headers = HeaderMap::new();

        for (k, v) in self.bucket().extra_headers.iter() {
            headers.insert(k.clone(), v.clone());
        }

        let host_header = self.host_header();

        headers.insert(HOST, host_header.parse()?);

        match self.command() {
            Command::CopyObject { from } => {
                headers.insert(HeaderName::from_static("x-amz-copy-source"), from.parse()?);
            }
            Command::ListObjects { .. } => {}
            Command::ListObjectsV2 { .. } => {}
            Command::GetObject => {}
            Command::GetObjectTagging => {}
            Command::GetBucketLocation => {}
            _ => {
                headers.insert(
                    CONTENT_LENGTH,
                    self.command().content_length().to_string().parse()?,
                );
                headers.insert(CONTENT_TYPE, self.command().content_type().parse()?);
            }
        }
        headers.insert(
            HeaderName::from_static("x-amz-content-sha256"),
            sha256.parse()?,
        );
        headers.insert(
            HeaderName::from_static("x-amz-date"),
            self.long_date()?.parse()?,
        );

        if let Some(session_token) = self.bucket().session_token()? {
            headers.insert(
                HeaderName::from_static("x-amz-security-token"),
                session_token.parse()?,
            );
        } else if let Some(security_token) = self.bucket().security_token()? {
            headers.insert(
                HeaderName::from_static("x-amz-security-token"),
                security_token.parse()?,
            );
        }

        if let Command::PutObjectTagging { tags } = self.command() {
            let digest = md5::compute(tags);
            let hash = general_purpose::STANDARD.encode(digest.as_ref());
            headers.insert(HeaderName::from_static("content-md5"), hash.parse()?);
        } else if let Command::PutObject { content, .. } = self.command() {
            let digest = md5::compute(content);
            let hash = general_purpose::STANDARD.encode(digest.as_ref());
            headers.insert(HeaderName::from_static("content-md5"), hash.parse()?);
        } else if let Command::UploadPart { content, .. } = self.command() {
            let digest = md5::compute(content);
            let hash = general_purpose::STANDARD.encode(digest.as_ref());
            headers.insert(HeaderName::from_static("content-md5"), hash.parse()?);
        } else if let Command::GetObject {} = self.command() {
            headers.insert(ACCEPT, "application/octet-stream".to_string().parse()?);
            // headers.insert(header::ACCEPT_CHARSET, HeaderValue::from_str("UTF-8")?);
        } else if let Command::GetObjectRange { start, end } = self.command() {
            headers.insert(ACCEPT, "application/octet-stream".to_string().parse()?);

            let mut range = format!("bytes={}-", start);

            if let Some(end) = end {
                range.push_str(&end.to_string());
            }

            headers.insert(RANGE, range.parse()?);
        } else if let Command::CreateBucket { ref config } = self.command() {
            config.add_headers(&mut headers)?;
        }

        // This must be last, as it signs the other headers, omitted if no secret key is provided
        if self.bucket().secret_key()?.is_some() {
            let authorization = self.authorization(&headers)?;
            headers.insert(AUTHORIZATION, authorization.parse()?);
        }

        // The format of RFC2822 is somewhat malleable, so including it in
        // signed headers can cause signature mismatches. We do include the
        // X-Amz-Date header, so requests are still properly limited to a date
        // range and can't be used again e.g. reply attacks. Adding this header
        // after the generation of the Authorization header leaves it out of
        // the signed headers.
        headers.insert(DATE, self.datetime().format(&Rfc2822)?.parse()?);

        Ok(headers)
    }
}

impl<'a> HyperRequest<'a> {
    pub fn new(
        bucket: &'a Bucket,
        path: &'a str,
        command: Command<'a>,
    ) -> Result<HyperRequest<'a>, S3Error> {
        bucket.credentials_refresh()?;
        Ok(Self {
            bucket,
            path,
            command,
            datetime: OffsetDateTime::now_utc(),
            sync: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::bucket::Bucket;
    use crate::command::Command;
    use crate::request::tokio_backend::HyperRequest;
    use crate::request::Request;
    use awscreds::Credentials;
    use http::header::{HOST, RANGE};

    // Fake keys - otherwise using Credentials::default will use actual user
    // credentials if they exist.
    fn fake_credentials() -> Credentials {
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let secert_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        Credentials::new(Some(access_key), Some(secert_key), None, None, None).unwrap()
    }

    #[test]
    fn url_uses_https_by_default() {
        let region = "custom-region".parse().unwrap();
        let bucket = Bucket::new("my-first-bucket", region, fake_credentials()).unwrap();
        let path = "/my-first/path";
        let request = HyperRequest::new(&bucket, path, Command::GetObject).unwrap();

        assert_eq!(request.url().unwrap().scheme(), "https");

        let headers = request.headers().unwrap();
        let host = headers.get(HOST).unwrap();

        assert_eq!(*host, "my-first-bucket.custom-region".to_string());
    }

    #[test]
    fn url_uses_https_by_default_path_style() {
        let region = "custom-region".parse().unwrap();
        let bucket = Bucket::new("my-first-bucket", region, fake_credentials())
            .unwrap()
            .with_path_style();
        let path = "/my-first/path";
        let request = HyperRequest::new(&bucket, path, Command::GetObject).unwrap();

        assert_eq!(request.url().unwrap().scheme(), "https");

        let headers = request.headers().unwrap();
        let host = headers.get(HOST).unwrap();

        assert_eq!(*host, "custom-region".to_string());
    }

    #[test]
    fn url_uses_scheme_from_custom_region_if_defined() {
        let region = "http://custom-region".parse().unwrap();
        let bucket = Bucket::new("my-second-bucket", region, fake_credentials()).unwrap();
        let path = "/my-second/path";
        let request = HyperRequest::new(&bucket, path, Command::GetObject).unwrap();

        assert_eq!(request.url().unwrap().scheme(), "http");

        let headers = request.headers().unwrap();
        let host = headers.get(HOST).unwrap();
        assert_eq!(*host, "my-second-bucket.custom-region".to_string());
    }

    #[test]
    fn url_uses_scheme_from_custom_region_if_defined_with_path_style() {
        let region = "http://custom-region".parse().unwrap();
        let bucket = Bucket::new("my-second-bucket", region, fake_credentials())
            .unwrap()
            .with_path_style();
        let path = "/my-second/path";
        let request = HyperRequest::new(&bucket, path, Command::GetObject).unwrap();

        assert_eq!(request.url().unwrap().scheme(), "http");

        let headers = request.headers().unwrap();
        let host = headers.get(HOST).unwrap();
        assert_eq!(*host, "custom-region".to_string());
    }

    #[test]
    fn test_get_object_range_header() {
        let region = "http://custom-region".parse().unwrap();
        let bucket = Bucket::new("my-second-bucket", region, fake_credentials())
            .unwrap()
            .with_path_style();
        let path = "/my-second/path";

        let request = HyperRequest::new(
            &bucket,
            path,
            Command::GetObjectRange {
                start: 0,
                end: None,
            },
        )
        .unwrap();
        let headers = request.headers().unwrap();
        let range = headers.get(RANGE).unwrap();
        assert_eq!(range, "bytes=0-");

        let request = HyperRequest::new(
            &bucket,
            path,
            Command::GetObjectRange {
                start: 0,
                end: Some(1),
            },
        )
        .unwrap();
        let headers = request.headers().unwrap();
        let range = headers.get(RANGE).unwrap();
        assert_eq!(range, "bytes=0-1");
    }
}
