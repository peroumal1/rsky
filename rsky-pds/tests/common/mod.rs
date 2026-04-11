pub mod mock_appview;

use anyhow::Result;
use diesel::{Connection, PgConnection};
use diesel_migrations::{embed_migrations, EmbeddedMigrations, MigrationHarness};
use http_auth_basic::Credentials;
use rocket::http::{ContentType, Header};
use rocket::local::asynchronous::Client;
use rocket::serde::json::json;
use rsky_common::env::env_str;
use rsky_lexicon::com::atproto::server::CreateInviteCodeOutput;
use rsky_pds::config::ServerConfig;
use rsky_pds::{build_rocket, AppViewConfig, RocketConfig};
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres;
use testcontainers_modules::postgres::Postgres;

const MIGRATIONS: EmbeddedMigrations = embed_migrations!();

/**
    Establish connection to the testcontainer postgres
*/
#[tracing::instrument(skip_all)]
pub fn establish_connection(database_url: &str) -> Result<PgConnection> {
    tracing::debug!("Establishing database connection");
    let result = PgConnection::establish(database_url).map_err(|error| {
        let context = format!("Error connecting to {database_url:?}");
        anyhow::Error::new(error).context(context)
    })?;

    Ok(result)
}

/**
    Fetch PDS_ADMIN_PASS to be used for creating initial accounts
*/
pub fn get_admin_token() -> String {
    let credentials = Credentials::new("admin", env_str("PDS_ADMIN_PASS").unwrap().as_str());
    credentials.as_http_header()
}

/**
    Starts a testcontainer for a postgres instance, and runs migrations from rsky-pds
*/
pub async fn get_postgres() -> ContainerAsync<Postgres> {
    let postgres = postgres::Postgres::default()
        .start()
        .await
        .expect("Valid postgres instance");
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let connection_string = format!("postgres://postgres:postgres@localhost:{port}/postgres",);
    let mut conn =
        establish_connection(connection_string.as_str()).expect("Connection  Established");
    conn.run_pending_migrations(MIGRATIONS).unwrap();
    postgres
}

/**
    Start Client for the RSky-PDS and have it use the provided postgres container
*/
pub async fn get_client(postgres: &ContainerAsync<Postgres>) -> Client {
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let connection_string = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    Client::untracked(
        build_rocket(Some(RocketConfig {
            db_url: connection_string,
            app_view: None,
        }))
        .await,
    )
    .await
    .expect("Valid Rocket instance")
}

pub async fn get_client_with_appview(
    postgres: &ContainerAsync<Postgres>,
    appview_url: String,
    appview_did: String,
) -> Client {
    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let connection_string = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    Client::untracked(
        build_rocket(Some(RocketConfig {
            db_url: connection_string,
            app_view: Some(AppViewConfig {
                url: appview_url,
                did: appview_did,
            }),
        }))
        .await,
    )
    .await
    .expect("Valid Rocket instance")
}

/// Create a session and return (did, access_jwt).
pub async fn create_session(client: &Client, email: &str, password: &str) -> (String, String) {
    use rocket::serde::json::json;
    use rsky_lexicon::com::atproto::server::CreateSessionOutput;

    let response = client
        .post("/xrpc/com.atproto.server.createSession")
        .header(ContentType::JSON)
        .body(json!({ "identifier": email, "password": password }).to_string())
        .dispatch()
        .await;

    let out = response
        .into_json::<CreateSessionOutput>()
        .await
        .expect("createSession response");
    (out.did, out.access_jwt)
}

/// Create an account that is fully active and can write records.
///
/// `create_account` passes a hardcoded DID in the request body. The PDS
/// treats that as an "import from another PDS" path and sets
/// `deactivated = true`. This makes `AccessStandardIncludeChecks` reject
/// subsequent write requests (createRecord, etc.) with HTTP 400.
///
/// This helper clears `deactivatedAt` via a direct DB UPDATE so the account
/// behaves like a freshly-provisioned, active account.
pub async fn create_active_account(
    client: &Client,
    postgres: &ContainerAsync<Postgres>,
) -> (String, String) {
    use diesel::prelude::*;

    let (email, password) = create_account(client).await;

    let port = postgres.get_host_port_ipv4(5432).await.unwrap();
    let db_url = format!("postgres://postgres:postgres@localhost:{port}/postgres");
    let mut conn = establish_connection(&db_url).expect("db connect for account activation");
    diesel::sql_query(
        r#"UPDATE pds.actor SET "deactivatedAt" = NULL WHERE did = 'did:plc:khvyd3oiw46vif5gm7hijslk'"#,
    )
    .execute(&mut conn)
    .expect("clear deactivatedAt for test account");

    (email, password)
}

/**
    Creates a mock account for testing purposes
*/
pub async fn create_account(client: &Client) -> (String, String) {
    let domain = client
        .rocket()
        .state::<ServerConfig>()
        .unwrap()
        .identity
        .service_handle_domains
        .first()
        .unwrap();
    let input = json!({
        "useCount": 1
    });

    let response = client
        .post("/xrpc/com.atproto.server.createInviteCode")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", get_admin_token()))
        .body(input.to_string())
        .dispatch()
        .await;
    let invite_code = response
        .into_json::<CreateInviteCodeOutput>()
        .await
        .unwrap()
        .code;

    let account_input = json!({
        "did": "did:plc:khvyd3oiw46vif5gm7hijslk",
        "email": "foo@example.com",
        "handle": format!("foo{domain}"),
        "password": "password",
        "inviteCode": invite_code
    });

    client
        .post("/xrpc/com.atproto.server.createAccount")
        .header(ContentType::JSON)
        .header(Header::new("Authorization", get_admin_token()))
        .body(account_input.to_string())
        .dispatch()
        .await;

    ("foo@example.com".to_string(), "password".to_string())
}
