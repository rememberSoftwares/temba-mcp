use crate::oauth::Session;
use futures::stream::BoxStream;
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::ClientJsonRpcMessage,
    transport::streamable_http_client::{
        StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    },
};
use std::{collections::HashMap, sync::Arc};
#[derive(Clone)]
pub struct Http {
    pub client: reqwest::Client,
    pub session: Option<Arc<Session>>,
    pub endpoint: Arc<str>,
}
impl Http {
    async fn bearer(
        &self,
        uri: &str,
    ) -> Result<Option<String>, StreamableHttpError<reqwest::Error>> {
        if uri != self.endpoint.as_ref() {
            return Err(StreamableHttpError::Io(std::io::Error::other(
                "Credential destination mismatch",
            )));
        }
        match &self.session {
            Some(s) => s.access().await.map(|v| Some(v.to_string())).map_err(|_| {
                StreamableHttpError::Io(std::io::Error::other("OAuth token unavailable; reconnect"))
            }),
            None => Ok(None),
        }
    }
}
impl StreamableHttpClient for Http {
    type Error = reqwest::Error;
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let token = self.bearer(&uri).await?;
        self.client
            .post_message(uri, message, session_id, token, headers)
            .await
    }
    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        limit: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let token = self.bearer(&uri).await?;
        self.client
            .post_message_with_max_sse_event_size(uri, message, session_id, token, headers, limit)
            .await
    }
    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let token = self.bearer(&uri).await?;
        self.client
            .delete_session(uri, session_id, token, headers)
            .await
    }
    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<
        BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
        StreamableHttpError<Self::Error>,
    > {
        let token = self.bearer(&uri).await?;
        self.client
            .get_stream(uri, session_id, last_event_id, token, headers)
            .await
    }
    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _: Option<String>,
        headers: HashMap<HeaderName, HeaderValue>,
        limit: usize,
    ) -> Result<
        BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>,
        StreamableHttpError<Self::Error>,
    > {
        let token = self.bearer(&uri).await?;
        self.client
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                token,
                headers,
                limit,
            )
            .await
    }
}
