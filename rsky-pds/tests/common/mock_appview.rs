use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A minimal HTTP/1.1 server that returns a fixed JSON body for every request.
///
/// The response always includes `atproto-repo-rev: 0`, which tells the PDS
/// read-after-write logic to treat all locally written records as "unseen"
/// and apply the munge. Bind to port 0 (OS-assigned) to avoid conflicts
/// between parallel tests.
pub struct MockAppView {
    /// Base URL of the mock, e.g. `http://127.0.0.1:54321`.
    pub url: String,
    /// DID injected into `RocketConfig.app_view.did`.
    pub did: String,
}

impl MockAppView {
    /// Start the mock and return immediately. The server runs in a background
    /// task that is dropped when the test's tokio runtime exits.
    ///
    /// `repo_rev` is the value sent in the `atproto-repo-rev` response header.
    /// Set it to the repo rev captured BEFORE the records you want to appear as
    /// "local writes", so that `get_records_since_rev` finds them.
    pub async fn start(response_body: serde_json::Value, repo_rev: impl Into<String>) -> Self {
        Self::start_inner(response_body, Some(repo_rev.into())).await
    }

    /// Start the mock without an `atproto-repo-rev` response header.
    /// This exercises the `rev = None` path in `read_after_write_internal`,
    /// which returns `HandlerPipeThrough` (raw AppView bytes, no envelope).
    pub async fn start_without_rev(response_body: serde_json::Value) -> Self {
        Self::start_inner(response_body, None).await
    }

    async fn start_inner(response_body: serde_json::Value, rev: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock AppView");
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");

        let body = response_body.to_string();
        tokio::spawn(async move {
            loop {
                if let Ok((stream, _)) = listener.accept().await {
                    let body = body.clone();
                    let rev = rev.clone();
                    tokio::spawn(async move {
                        serve_once(stream, body, rev).await;
                    });
                }
            }
        });

        MockAppView {
            url,
            did: "did:web:mock-appview.test".to_string(),
        }
    }
}

async fn serve_once(mut stream: tokio::net::TcpStream, body: String, rev: Option<String>) {
    // Drain the request (we don't inspect it).
    let mut buf = [0u8; 8192];
    let _ = stream.read(&mut buf).await;

    let rev_header = match rev {
        Some(rev) => format!("atproto-repo-rev: {rev}\r\n"),
        None => String::new(),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         {rev_header}\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
        body = body,
    );
    let _ = stream.write_all(response.as_bytes()).await;
}
