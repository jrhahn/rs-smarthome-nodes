//! `smarthome-timeseries` -- the long-term memory of the sensor fleet.
//!
//! Home Assistant already sees every node over MQTT auto-discovery, and it
//! keeps a recorder database of its own. What it does not do is keep three
//! years of it: its recorder is tuned for weeks, and the long-run questions
//! this fleet was built to answer -- did insulating the roof change the
//! bedroom's overnight CO2, how does the terrace's temperature swing compare
//! between summers -- need the history to still be there.
//!
//! So this sits beside Home Assistant rather than inside it: one process that
//!
//!   1. follows the same MQTT topics the nodes already publish,
//!   2. writes every reading into QuestDB, which keeps three years of them and
//!      pre-aggregates the history into rollup views, and
//!   3. serves a small dashboard that reads those rollups.
//!
//! Nothing here touches the firmware or Home Assistant's configuration. A node
//! does not know it is being archived, and switching this service off leaves
//! the house working exactly as before.

mod config;
mod model;
mod mqtt;
mod questdb;
mod state;
mod web;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use config::Settings;
use questdb::{schema, Client, Record};
use state::Shared;

/// How long to keep waiting for a database that is not up yet.
///
/// On the home server this process and QuestDB come up together, and systemd's
/// ordering only guarantees that the unit *started*, not that it is answering
/// queries -- QuestDB takes a few seconds to open its tables. Retrying beats
/// failing and being restarted, because a restart loop loses the MQTT
/// subscription too.
const DB_WAIT: Duration = Duration::from_secs(180);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("SMARTHOME_TIMESERIES_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let path = settings_path();
    let settings = Settings::load(path.as_deref())?;
    match &path {
        Some(p) => info!(config = %p.display(), "settings loaded"),
        None => info!("no settings file given; using defaults"),
    }

    let client = Client::new(
        settings.questdb_base(),
        &settings.questdb.user,
        &settings.questdb.password,
    )?;
    wait_for_database(&client, settings.questdb_base()).await?;

    let views = schema::ensure(
        &client,
        &settings.questdb.table,
        &settings.questdb.status_table,
        &settings.retention(),
        &settings.rollup_retention(),
        settings.questdb.rollups,
    )
    .await
    .context("preparing the QuestDB schema")?;

    let shared = Shared::new();
    shared.set_views(views);

    // What the database already knows about who is online, so the retained
    // last-wills that arrive on connect are recognised as old news.
    match questdb::series::fetch_status(&client, &settings.questdb.status_table).await {
        Ok(rows) => {
            for (node, online) in &rows {
                shared.seed_online(node, *online);
            }
            info!(nodes = rows.len(), "seeded availability from the database");
        }
        Err(e) => warn!(error = %e, "could not read the last known availability"),
    }

    // Bounded: if QuestDB stops answering, back-pressure should reach the MQTT
    // loop rather than growing a queue until the machine swaps. Readings the
    // bridge cannot take are lost, which on a live feed is the right trade --
    // the alternative is losing all of them to an out-of-memory kill.
    let (tx, rx) = mpsc::channel::<Record>(4_096);

    let writer = tokio::spawn(questdb::writer::run(
        client.clone(),
        settings.questdb.table.clone(),
        settings.questdb.status_table.clone(),
        Duration::from_secs(settings.questdb.flush_interval_secs),
        settings.questdb.batch_max,
        shared.stats(),
        rx,
    ));

    let bridge = tokio::spawn(mqtt::run(settings.clone(), shared.clone(), tx));

    let app = web::App {
        settings: Arc::new(settings.clone()),
        client,
        shared,
    };
    let listener = tokio::net::TcpListener::bind(settings.web.bind)
        .await
        .with_context(|| format!("binding {}", settings.web.bind))?;
    info!(address = %settings.web.bind, "dashboard listening");

    axum::serve(listener, web::router(app))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving the dashboard")?;

    // The bridge has no shutdown of its own -- it is a reconnect loop with no
    // state worth saving. The writer does: dropping the sender closes the
    // channel, which makes it flush what is buffered and return.
    bridge.abort();
    info!("flushing the last batch");
    match tokio::time::timeout(Duration::from_secs(30), writer).await {
        Ok(Ok(())) => info!("writer stopped cleanly"),
        Ok(Err(e)) => error!(error = %e, "writer task failed"),
        Err(_) => warn!("writer did not finish within 30 s; exiting anyway"),
    }
    Ok(())
}

/// The settings file: first argument, or `$SMARTHOME_TIMESERIES_CONFIG`, or
/// nothing at all (every default works against a localhost broker and
/// database).
fn settings_path() -> Option<PathBuf> {
    std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SMARTHOME_TIMESERIES_CONFIG").ok())
        .map(PathBuf::from)
}

async fn wait_for_database(client: &Client, url: &str) -> Result<()> {
    let deadline = std::time::Instant::now() + DB_WAIT;
    let mut last_error = None;
    let mut announced = false;
    while std::time::Instant::now() < deadline {
        match client.ping().await {
            Ok(()) => {
                info!(url, "QuestDB is up");
                return Ok(());
            }
            Err(e) => {
                if !announced {
                    info!(url, "waiting for QuestDB");
                    announced = true;
                }
                last_error = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last_error
        .unwrap_or_else(|| anyhow::anyhow!("no error recorded"))
        .context(format!(
            "QuestDB at {url} did not answer within {DB_WAIT:?}"
        )))
}

/// Ctrl-C interactively, `SIGTERM` under systemd.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                warn!(error = %e, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("interrupted"),
        _ = terminate => info!("terminated"),
    }
}
