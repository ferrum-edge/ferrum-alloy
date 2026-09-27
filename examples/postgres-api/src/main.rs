//! `example-postgres-api` serves the API; `example-postgres-api migrate`
//! applies migrations and exits. Migrations never run implicitly.

use ferrum_alloy::{AlloyApp, postgres};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut app = AlloyApp::new("orders-api")
        .version(env!("CARGO_PKG_VERSION"))
        .config_file(concat!(env!("CARGO_MANIFEST_DIR"), "/alloy.toml"));
    let config = app.prepare()?;
    let pool = postgres::connect(&config.database, "orders-api")?;

    if std::env::args().nth(1).as_deref() == Some("migrate") {
        postgres::migrate(&pool, &example_postgres_api::MIGRATOR).await?;
        return Ok(());
    }

    let (router, document) = example_postgres_api::api(pool.clone());
    app.router(router)
        .openapi(&document)
        .readiness_check("postgres", postgres::readiness(pool))
        .run()
        .await?;
    Ok(())
}
