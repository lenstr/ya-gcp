//! An API for interacting with [Google Cloud Storage](https://cloud.google.com/storage).
use std::convert::TryFrom;

pub use api::objects::Metadata;
pub use hyper::body::Bytes;
use hyper::client::Client;
pub use tame_gcs as api;
use tame_gcs::{
    objects,
    types::{BucketName, ObjectId, ObjectName},
    ApiResponse,
};

use crate::builder;

const READ_WRITE_SCOPE: &str = "https://www.googleapis.com/auth/devstorage.read_write";

/// Convert an empty request from the type produced by tame-gcs to one accepted by hyper
fn empty_body(req: http::Request<std::io::Empty>) -> http::Request<hyper::Body> {
    let (parts, std::io::Empty { .. }) = req.into_parts();
    http::Request::from_parts(parts, hyper::Body::empty())
}

/// Attempt to collect the bytes of a response's body stream into a single allocation of bytes
async fn collect_body(
    response: http::Response<hyper::Body>,
) -> Result<http::Response<Bytes>, hyper::Error> {
    let (parts, body) = response.into_parts();

    let bytes = hyper::body::to_bytes(body).await?;

    Ok(http::Response::from_parts(parts, bytes))
}

/// An error indicating that a given object name was invalid
#[derive(Debug, thiserror::Error)]
pub enum InvalidNameError {
    /// The given name did not have a valid bucket
    #[error("error validating bucket name {1}")]
    Bucket(#[source] api::Error, String),

    /// The given name did not have a valid object
    #[error("error validating object name {1}")]
    Object(#[source] api::Error, String),
}

fn names_to_object<'a>(
    bucket_name: &'a str,
    object_name: &'a str,
) -> Result<ObjectId<'a>, InvalidNameError> {
    let bucket = BucketName::try_from(bucket_name)
        .map_err(|e| InvalidNameError::Bucket(e, bucket_name.to_owned()))?;
    let object = ObjectName::try_from(object_name)
        .map_err(|e| InvalidNameError::Object(e, object_name.to_owned()))?;

    Ok(ObjectId { bucket, object })
}

/// An error in getting an authorization token
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// An error in fetching the auth token
    #[error("error in fetching auth token")]
    Fetch(#[source] yup_oauth2::Error),

    /// An error in validating a received auth token
    #[error("token does not form a valid HTTP header value: {}", _1.as_str())]
    InvalidToken(
        #[source] http::header::InvalidHeaderValue,
        yup_oauth2::AccessToken,
    ),
}

/// Errors that could be encountered when reading or writing objects to storage
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ObjectError {
    /// The given object name was invalid
    #[error(transparent)]
    InvalidName(#[from] InvalidNameError),

    /// The request could not be formed
    #[error("error in creating request")]
    InvalidRequest(#[source] api::Error),

    /// An auth token could not be retrieved
    #[error(transparent)]
    Auth(#[from] AuthError),

    /// The request could not be sent
    #[error("error in sending request")]
    Request(#[source] hyper::Error),

    /// The response could not be received
    #[error("error in receiving response")]
    Response(#[source] hyper::Error),

    /// The response indicated some invalid state
    #[error("received unsuccessful response")]
    Failure(#[source] api::Error),
}

/// A client used to interact with Google Cloud Storage
pub struct StorageClient<C = builder::DefaultConnector> {
    client: Client<C>,
    auth: Option<crate::Auth<C>>,
}

impl<C> StorageClient<C>
where
    C: hyper::client::connect::Connect + Clone + Send + Sync + 'static,
{
    /// Add authentication to the request and send it, awaiting the response. The response will be
    /// collected into a single memory allocation (not a streamed body)
    async fn send_request(
        &self,
        mut request: http::Request<hyper::Body>,
    ) -> Result<http::Response<Bytes>, ObjectError> {
        if let Some(auth) = &self.auth {
            let auth_token = auth
                .token(&[READ_WRITE_SCOPE])
                .await
                .map_err(AuthError::Fetch)?;

            crate::auth::add_auth_token(&mut request, &auth_token)
                .map_err(|e| AuthError::InvalidToken(e, auth_token))?;
        }

        let response = self
            .client
            .request(request)
            .await
            .map_err(ObjectError::Request)?;

        let response_bytes = collect_body(response)
            .await
            .map_err(ObjectError::Response)?;

        Ok(response_bytes)
    }

    /// Get the contents of an object in storage
    pub async fn get_object(
        &self,
        bucket_name: impl AsRef<str>,
        object_name: impl AsRef<str>,
    ) -> Result<Bytes, ObjectError> {
        let oid = names_to_object(bucket_name.as_ref(), object_name.as_ref())?;

        let request = objects::Object::download(&oid, None).map_err(ObjectError::InvalidRequest)?;

        let response = self.send_request(empty_body(request)).await?;

        Ok(objects::DownloadObjectResponse::try_from_parts(response)
            .map_err(ObjectError::Failure)?
            .consume())
    }

    /// Get the metadata of an object in storage
    pub async fn get_metadata(
        &self,
        bucket_name: impl AsRef<str>,
        object_name: impl AsRef<str>,
    ) -> Result<Metadata, ObjectError> {
        let oid = names_to_object(bucket_name.as_ref(), object_name.as_ref())?;

        let request = objects::Object::get(&oid, None).map_err(ObjectError::InvalidRequest)?;

        let response = self.send_request(empty_body(request)).await?;

        Ok(objects::GetObjectResponse::try_from_parts(response)
            .map_err(ObjectError::Failure)?
            .metadata)
    }

    /// Store the given data as an object in storage without any additional metadata
    ///
    /// Returns the metadata of the newly written object if successful.
    pub async fn insert_object(
        &self,
        bucket_name: impl AsRef<str>,
        object_name: impl AsRef<str>,
        data: impl Into<Bytes>,
    ) -> Result<Metadata, ObjectError> {
        let oid = names_to_object(bucket_name.as_ref(), object_name.as_ref())?;

        let data: Bytes = data.into();
        let data_len = data.len();
        let body = hyper::Body::from(data);

        let request = objects::Object::insert_simple(
            &oid,
            body,
            u64::try_from(data_len).expect("data length should fit in u64"),
            None,
        )
        .map_err(ObjectError::InvalidRequest)?;

        let response = self.send_request(request).await?;

        Ok(objects::InsertResponse::try_from_parts(response)
            .map_err(ObjectError::Failure)?
            .metadata)
    }

    /// Store the given data as an object in storage, together with the associated metadata.
    ///
    /// Note that the object name must be provided as part of the metadata; other metadata fields
    /// documented as _writable_ may also be specified.
    ///
    /// ```no_run
    /// use ya_gcp::storage;
    ///
    /// # async {
    /// let client: storage::StorageClient = // ...
    /// # unimplemented!();
    /// let bucket = "my-bucket";
    /// let data = "my data";
    /// let written_metadata = storage::Metadata {
    ///     name: Some("my-object".to_owned()),
    ///     content_type: Some("text/plain; charset=utf-8".to_owned()),
    ///     storage_class: Some(storage::api::common::StorageClass::MultiRegional),
    ///     ..storage::Metadata::default()
    /// };
    ///
    /// let returned_metadata = client
    ///     .insert_with_metadata(bucket, data, &written_metadata, None)
    ///     .await?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// # };
    /// ```
    ///
    /// Returns the full metadata of the newly written object if successful.
    pub async fn insert_with_metadata(
        &self,
        bucket_name: impl AsRef<str>,
        data: impl Into<Bytes>,
        metadata: &Metadata,
        optional: Option<objects::InsertObjectOptional<'_>>,
    ) -> Result<Metadata, ObjectError> {
        let bucket = BucketName::try_from(bucket_name.as_ref())
            .map_err(|e| InvalidNameError::Bucket(e, bucket_name.as_ref().to_owned()))?;

        let data: Bytes = data.into();
        let data_len = data.len();

        let request = objects::Object::insert_multipart(
            &bucket,
            std::io::Cursor::new(data),
            u64::try_from(data_len).expect("data length should fit in u64"),
            metadata,
            optional,
        )
        .map_err(ObjectError::InvalidRequest)?;

        // unfortunately there isn't a good way to get from tame_gcs::Multipart's std::io::Read
        // impl into a hyper::Body without copying. This includes copying the data, which is
        // potentially large
        let (parts, mut content) = request.into_parts();
        let mut buf = Vec::with_capacity(
            usize::try_from(content.total_len()).expect("content length should fit in usize"),
        );

        <objects::Multipart<std::io::Cursor<Bytes>> as std::io::Read>::read_to_end(
            &mut content,
            &mut buf,
        )
        .expect("in-memory read should not error");

        let request = http::Request::from_parts(parts, hyper::Body::from(buf));

        let response = self.send_request(request).await?;

        Ok(objects::InsertResponse::try_from_parts(response)
            .map_err(ObjectError::Failure)?
            .metadata)
    }
}

impl<C> builder::ClientBuilder<C>
where
    C: Clone,
{
    /// Create a client for access Google Cloud Storage
    pub fn build_storage_client(&self) -> StorageClient<C> {
        StorageClient {
            client: self.client.clone(),
            auth: self.auth.clone(),
        }
    }
}
