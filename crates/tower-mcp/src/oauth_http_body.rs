/// Allow room for large tokens, metadata, and key rotations while bounding
/// the bytes buffered for each OAuth or JWKS response (including errors).
const MAX_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum BodyError {
    #[error("response body exceeds the {MAX_BODY_BYTES}-byte limit")]
    TooLarge,
    #[error(transparent)]
    Read(reqwest::Error),
}

/// Buffer only complete bodies within the limit. The stream still needs to
/// reach EOF at the limit, since its next chunk could take it over budget.
pub(crate) async fn read(mut response: reqwest::Response) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BODY_BYTES as u64)
    {
        return Err(BodyError::TooLarge);
    }

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| BodyError::Read(error.with_url(response.url().clone())))?
    {
        if chunk.len() > MAX_BODY_BYTES - body.len() {
            return Err(BodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepts_a_body_at_the_limit() {
        let response = http::Response::new(vec![b' '; MAX_BODY_BYTES]).into();
        assert_eq!(read(response).await.unwrap(), vec![b' '; MAX_BODY_BYTES]);
    }

    #[tokio::test]
    async fn rejects_a_declared_body_over_the_limit() {
        let response: reqwest::Response =
            http::Response::new(vec![b' '; MAX_BODY_BYTES + 1]).into();
        assert_eq!(response.content_length(), Some(MAX_BODY_BYTES as u64 + 1));
        assert!(matches!(read(response).await, Err(BodyError::TooLarge)));
    }

    #[tokio::test]
    async fn rejects_a_streamed_error_body_over_the_limit() {
        let chunks = [vec![b' '; MAX_BODY_BYTES], vec![b' ']];
        let stream = futures::stream::iter(chunks.map(Ok::<_, std::io::Error>));
        let response: reqwest::Response = http::Response::builder()
            .status(http::StatusCode::BAD_REQUEST)
            .body(reqwest::Body::wrap_stream(stream))
            .unwrap()
            .into();
        assert_eq!(response.content_length(), None);
        assert!(matches!(read(response).await, Err(BodyError::TooLarge)));
    }
}
