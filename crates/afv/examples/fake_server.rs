//! Runs the in-process fake AFV server used by the tests, with echo enabled: every transmission
//! is played back as station `ECHO` half a second after it ends. Pairs with the pilot example:
//!
//! ```text
//! cargo run -p afv --example fake_server
//! cargo run -p afv --features cpal --example pilot -- --api-url <printed URL>
//! ```

#[path = "../tests/support/mod.rs"]
mod support;

use std::sync::atomic::Ordering::SeqCst;

#[tokio::main]
async fn main() {
    let server = support::FakeServer::start().await;
    server.fake.echo.store(true, SeqCst);
    println!("fake AFV server at {}", server.api_url);
    println!("login {} / {}", support::USERNAME, support::PASSWORD);
    std::future::pending::<()>().await;
}
