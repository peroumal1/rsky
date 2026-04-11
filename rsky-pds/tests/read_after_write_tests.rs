/// Integration tests for the read-after-write munge layer.
///
/// These tests spin up a real PDS (Rocket + Postgres via testcontainers) wired
/// to a mock AppView HTTP server. The mock returns controlled JSON so we can
/// assert that the PDS correctly merges local state on top of the upstream
/// AppView response before returning it to the client.
///
/// Coverage: PR #175 — `postsCount` is incremented by the number of posts
/// written locally after the revision the AppView last processed.
///
/// Response shape reference
/// ------------------------
/// When the munge is applied, `getProfile` returns a `HandlerResponse` envelope:
///   { "encoding": "application/json", "body": { ...profile... }, "headers": {...} }
/// so the profile fields live at `body["body"][...]`.
///
/// When the munge is skipped (unauthenticated, AppView already current, or no
/// local "self" profile), the raw AppView bytes are forwarded as-is
/// (`HandlerPipeThrough`), so profile fields live directly at `body[...]`.
use rocket::http::{ContentType, Header, Status};
use rsky_lexicon::com::atproto::sync::GetLatestCommitOutput;
use serde_json::json;

mod common;

/// After a user writes a profile record and a post locally, a `getProfile`
/// call should return `postsCount` equal to the upstream value PLUS the
/// number of locally written posts that the AppView hasn't yet indexed.
///
/// Sequence
/// --------
/// 1.  Create account (no records in `record` table yet).
/// 2.  Write a *sentinel* post — this is the "oldest" record the AppView has
///     seen. Its `repoRev` (TID_sentinel) satisfies the sanity check inside
///     `get_records_since_rev`, which requires at least one record with
///     `repoRev ≤ rev`.
/// 3.  Capture the current head rev via `com.atproto.sync.getLatestCommit`.
///     This returns TID_sentinel.
/// 4.  Write the profile record (rkey "self") → `repoRev = TID_profile`.
/// 5.  Write the test post → `repoRev = TID_post`.
/// 6.  Start the mock AppView returning `postsCount: 5` and
///     `atproto-repo-rev: TID_sentinel`.
/// 7.  Call `getProfile` through the appview-wired PDS client.
///
/// Expected result: `postsCount = 6` (5 upstream + 1 local post after sentinel
/// rev). The sentinel post is NOT counted because its `repoRev == TID_sentinel`
/// which fails the `> rev` filter.
#[tokio::test]
async fn get_profile_postscount_incremented_after_local_create_record() {
    // ── 1. Bootstrap: create account ─────────────────────────────────────────

    let postgres = common::get_postgres().await;
    let bootstrap = common::get_client(&postgres).await;

    let (email, password) = common::create_active_account(&bootstrap, &postgres).await;
    let (did, token) = common::create_session(&bootstrap, &email, &password).await;
    let auth = format!("Bearer {token}");

    // ── 2. Write sentinel record ──────────────────────────────────────────────
    // Any collection works. This record's repoRev becomes the AppView's
    // "already seen" rev, so it must exist in the `record` table.

    let sentinel_resp = bootstrap
        .post("/xrpc/com.atproto.repo.createRecord")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", auth.clone()))
        .body(
            json!({
                "repo": did,
                "collection": "app.bsky.feed.post",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "sentinel — not counted in postsCount test",
                    "createdAt": "2024-01-01T00:00:00.000Z"
                }
            })
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(sentinel_resp.status(), Status::Ok, "sentinel createRecord");

    // ── 3. Capture current head rev ───────────────────────────────────────────

    let commit_resp = bootstrap
        .get(format!("/xrpc/com.atproto.sync.getLatestCommit?did={did}"))
        .dispatch()
        .await;
    assert_eq!(commit_resp.status(), Status::Ok, "getLatestCommit");

    let commit: GetLatestCommitOutput =
        commit_resp.into_json().await.expect("getLatestCommit JSON");
    let sentinel_rev = commit.rev;

    // ── 4. Write profile record (rkey "self") ─────────────────────────────────
    // Required for get_profile_munge to call update_profile_detailed
    // (local.profile must be Some).

    let profile_resp = bootstrap
        .post("/xrpc/com.atproto.repo.createRecord")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", auth.clone()))
        .body(
            json!({
                "repo": did,
                "collection": "app.bsky.actor.profile",
                "rkey": "self",
                "record": {
                    "$type": "app.bsky.actor.profile",
                    "displayName": "Test User"
                }
            })
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(profile_resp.status(), Status::Ok, "createRecord (profile)");

    // ── 5. Write test post ────────────────────────────────────────────────────

    let post_resp = bootstrap
        .post("/xrpc/com.atproto.repo.createRecord")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", auth.clone()))
        .body(
            json!({
                "repo": did,
                "collection": "app.bsky.feed.post",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "read-after-write integration test post",
                    "createdAt": "2024-01-02T00:00:00.000Z"
                }
            })
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(post_resp.status(), Status::Ok, "createRecord (test post)");

    // ── 6. Start mock AppView ─────────────────────────────────────────────────
    // Returns postsCount: 5 with atproto-repo-rev = sentinel_rev.
    // The PDS will find profile + test post as local records (repoRev >
    // sentinel_rev) and apply the munge: postsCount = 5 + 1 = 6.

    let mock = common::mock_appview::MockAppView::start(
        json!({
            "did": did,
            "handle": "foo.test",
            "displayName": null,
            "description": null,
            "avatar": null,
            "banner": null,
            "followersCount": 0,
            "followsCount": 0,
            "postsCount": 5,
            "labels": [],
            "indexedAt": "2024-01-01T00:00:00.000Z"
        }),
        sentinel_rev,
    )
    .await;

    // ── 7. Call getProfile through the appview-wired client ───────────────────

    let client =
        common::get_client_with_appview(&postgres, mock.url.clone(), mock.did.clone()).await;

    let get_resp = client
        .get(format!("/xrpc/app.bsky.actor.getProfile?actor={did}"))
        .header(Header::new("Authorization", auth.clone()))
        .dispatch()
        .await;

    assert_eq!(get_resp.status(), Status::Ok, "getProfile must succeed");

    let body: serde_json::Value = get_resp
        .into_json()
        .await
        .expect("getProfile must return valid JSON");

    // The munge path returns HandlerResponse<T> which serialises as
    // { "encoding": "...", "body": { ...profile... }, "headers": {...} }.
    // Access the nested profile body.
    assert_eq!(
        body["body"]["postsCount"], 6,
        "postsCount should be upstream (5) + 1 local post after sentinel rev = 6, got: {body}"
    );
}

/// When the AppView response contains no `atproto-repo-rev` header,
/// `read_after_write_internal` treats `rev` as `None` and returns
/// `HandlerPipeThrough` immediately — the raw AppView bytes are forwarded
/// with no envelope wrapper.
///
/// Path: `inner_get_profile` → `read_after_write_internal` →
///       `rev = None` → `HandlerPipeThrough`.
#[tokio::test]
async fn get_profile_no_rev_header_returns_raw_appview_response() {
    let postgres = common::get_postgres().await;
    let bootstrap = common::get_client(&postgres).await;

    let (email, password) = common::create_active_account(&bootstrap, &postgres).await;
    let (did, token) = common::create_session(&bootstrap, &email, &password).await;
    let auth = format!("Bearer {token}");

    // Mock returns no `atproto-repo-rev` header → rev = None → PipeThrough.
    let mock = common::mock_appview::MockAppView::start_without_rev(json!({
        "did": did,
        "handle": "foo.test",
        "displayName": null,
        "description": null,
        "avatar": null,
        "banner": null,
        "followersCount": 0,
        "followsCount": 0,
        "postsCount": 5,
        "labels": [],
        "indexedAt": "2024-01-01T00:00:00.000Z"
    }))
    .await;

    let client =
        common::get_client_with_appview(&postgres, mock.url.clone(), mock.did.clone()).await;

    let resp = client
        .get(format!("/xrpc/app.bsky.actor.getProfile?actor={did}"))
        .header(Header::new("Authorization", auth.clone()))
        .dispatch()
        .await;

    assert_eq!(resp.status(), Status::Ok, "getProfile must succeed");

    let body: serde_json::Value = resp
        .into_json()
        .await
        .expect("getProfile must return valid JSON");

    // HandlerPipeThrough: raw AppView JSON forwarded as-is, no envelope.
    assert_eq!(
        body["postsCount"], 5,
        "no rev header: postsCount must equal raw upstream value, got: {body}"
    );
}

/// When the AppView's `atproto-repo-rev` equals the current repo HEAD, there
/// are no local writes the AppView hasn't seen yet (`local.count == 0`).
///
/// Path: `read_after_write_internal` → `local.count <= 0` → `HandlerPipeThrough`.
///
/// The raw AppView bytes are forwarded unchanged; `postsCount` stays at the
/// upstream value and lives at the top level (no envelope).
#[tokio::test]
async fn get_profile_appview_current_skips_munge() {
    let postgres = common::get_postgres().await;
    let bootstrap = common::get_client(&postgres).await;

    let (email, password) = common::create_active_account(&bootstrap, &postgres).await;
    let (did, token) = common::create_session(&bootstrap, &email, &password).await;
    let auth = format!("Bearer {token}");

    // Write a post so the repo has at least one commit.
    let post_resp = bootstrap
        .post("/xrpc/com.atproto.repo.createRecord")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", auth.clone()))
        .body(
            json!({
                "repo": did,
                "collection": "app.bsky.feed.post",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "appview-current test post",
                    "createdAt": "2024-01-01T00:00:00.000Z"
                }
            })
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(post_resp.status(), Status::Ok, "createRecord");

    // Capture the HEAD rev *after* all writes — the AppView is fully current.
    let commit_resp = bootstrap
        .get(format!("/xrpc/com.atproto.sync.getLatestCommit?did={did}"))
        .dispatch()
        .await;
    assert_eq!(commit_resp.status(), Status::Ok, "getLatestCommit");
    let commit: GetLatestCommitOutput =
        commit_resp.into_json().await.expect("getLatestCommit JSON");
    let current_rev = commit.rev;

    // Mock returns the current HEAD rev → no local records after it.
    let mock = common::mock_appview::MockAppView::start(
        json!({
            "did": did,
            "handle": "foo.test",
            "displayName": null,
            "description": null,
            "avatar": null,
            "banner": null,
            "followersCount": 0,
            "followsCount": 0,
            "postsCount": 5,
            "labels": [],
            "indexedAt": "2024-01-01T00:00:00.000Z"
        }),
        current_rev,
    )
    .await;

    let client =
        common::get_client_with_appview(&postgres, mock.url.clone(), mock.did.clone()).await;

    let resp = client
        .get(format!("/xrpc/app.bsky.actor.getProfile?actor={did}"))
        .header(Header::new("Authorization", auth.clone()))
        .dispatch()
        .await;

    assert_eq!(resp.status(), Status::Ok, "getProfile must succeed");

    let body: serde_json::Value = resp
        .into_json()
        .await
        .expect("getProfile must return valid JSON");

    // HandlerPipeThrough: raw AppView JSON, no envelope.
    assert_eq!(
        body["postsCount"], 5,
        "when AppView is current, postsCount must not be incremented, got: {body}"
    );
}

/// When the user has written posts locally that the AppView hasn't indexed yet
/// but has NOT written a local `app.bsky.actor.profile` record (rkey "self"),
/// `get_profile_munge` exits early via `local.profile = None`.
///
/// Path: `read_after_write_internal` → munge entered (local.count > 0) →
/// `get_profile_munge` → `local.profile = None` → returns original unchanged →
/// `HandlerResponse` envelope with upstream `postsCount`.
#[tokio::test]
async fn get_profile_no_local_profile_record_postscount_unchanged() {
    let postgres = common::get_postgres().await;
    let bootstrap = common::get_client(&postgres).await;

    let (email, password) = common::create_active_account(&bootstrap, &postgres).await;
    let (did, token) = common::create_session(&bootstrap, &email, &password).await;
    let auth = format!("Bearer {token}");

    // Write a sentinel post to satisfy the get_records_since_rev sanity check.
    let sentinel_resp = bootstrap
        .post("/xrpc/com.atproto.repo.createRecord")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", auth.clone()))
        .body(
            json!({
                "repo": did,
                "collection": "app.bsky.feed.post",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "sentinel post",
                    "createdAt": "2024-01-01T00:00:00.000Z"
                }
            })
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(sentinel_resp.status(), Status::Ok, "sentinel createRecord");

    let commit_resp = bootstrap
        .get(format!("/xrpc/com.atproto.sync.getLatestCommit?did={did}"))
        .dispatch()
        .await;
    let commit: GetLatestCommitOutput =
        commit_resp.into_json().await.expect("getLatestCommit JSON");
    let sentinel_rev = commit.rev;

    // Write a post AFTER sentinel_rev but deliberately skip writing a "self"
    // profile record — local.profile will be None.
    let post_resp = bootstrap
        .post("/xrpc/com.atproto.repo.createRecord")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", auth.clone()))
        .body(
            json!({
                "repo": did,
                "collection": "app.bsky.feed.post",
                "record": {
                    "$type": "app.bsky.feed.post",
                    "text": "local post, no profile record written",
                    "createdAt": "2024-01-02T00:00:00.000Z"
                }
            })
            .to_string(),
        )
        .dispatch()
        .await;
    assert_eq!(post_resp.status(), Status::Ok, "createRecord (post)");

    let mock = common::mock_appview::MockAppView::start(
        json!({
            "did": did,
            "handle": "foo.test",
            "displayName": null,
            "description": null,
            "avatar": null,
            "banner": null,
            "followersCount": 0,
            "followsCount": 0,
            "postsCount": 5,
            "labels": [],
            "indexedAt": "2024-01-01T00:00:00.000Z"
        }),
        sentinel_rev,
    )
    .await;

    let client =
        common::get_client_with_appview(&postgres, mock.url.clone(), mock.did.clone()).await;

    let resp = client
        .get(format!("/xrpc/app.bsky.actor.getProfile?actor={did}"))
        .header(Header::new("Authorization", auth.clone()))
        .dispatch()
        .await;

    assert_eq!(resp.status(), Status::Ok, "getProfile must succeed");

    let body: serde_json::Value = resp
        .into_json()
        .await
        .expect("getProfile must return valid JSON");

    // local.count > 0 (one post after sentinel_rev) → munge is entered and
    // response is wrapped in HandlerResponse. But local.profile = None →
    // munge returns original unchanged → postsCount stays at upstream value.
    assert_eq!(
        body["body"]["postsCount"], 5,
        "without a local profile record, postsCount must not be incremented, got: {body}"
    );
}
