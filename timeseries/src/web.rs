//! The dashboard: four JSON endpoints and three static files.
//!
//! There is no build step and no bundler. The page is served from
//! `include_str!` of the files next door, which means the binary is the whole
//! deployment -- copy it to the home server, point it at the broker, done --
//! and that a running service cannot be half-upgraded, with a new binary
//! serving the old page out of a stale asset directory.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::config::Settings;
use crate::model::{now_micros, Channel, Micros};
use crate::questdb::{annotations, series, Client};
use crate::state::{Shared, StatsSnapshot};

#[derive(Clone)]
pub struct App {
    pub settings: Arc<Settings>,
    pub client: Client,
    pub shared: Shared,
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/style.css", get(stylesheet))
        .route("/api/channels", get(channels))
        .route("/api/series", get(series_handler))
        .route("/api/overview", get(overview))
        .route(
            "/api/annotations",
            get(annotations_handler).post(add_annotation),
        )
        .route("/api/annotations/void", post(void_annotation))
        .route("/api/health", get(health))
        .with_state(app)
}

/// Anything that goes wrong below becomes a 500 with the reason in it.
///
/// The reason is shown in the dashboard's footer rather than swallowed: on a
/// home server the person reading the chart is the person who can fix the
/// database, and "QuestDB rejected ... : table does not exist" is the whole
/// diagnosis.
struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        warn!(error = %self.0, "request failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        include_str!("../assets/index.html"),
    )
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/app.js"),
    )
}

async fn stylesheet() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/style.css"),
    )
}

/// Every channel, with its labels and its current value.
///
/// The list is the union of three sources, because each knows something the
/// others do not: QuestDB knows every channel that ever stored a row (including
/// nodes that are switched off today), the retained discovery messages know the
/// names and units, and the bridge's memory knows what arrived in the last few
/// seconds -- a brand-new sensor appears here before its first row is flushed.
async fn channels(State(app): State<App>) -> ApiResult<Json<Vec<Channel>>> {
    let base = &app.settings.questdb.table;
    let views = app.shared.views();

    let mut keys: Vec<(String, String)> = series::fetch_channels(&app.client, base, &views)
        .await?
        .into_iter()
        .map(|(node, sensor, _)| (node, sensor))
        .collect();
    for known in app.shared.known_channels() {
        if !keys.contains(&known) {
            keys.push(known);
        }
    }

    // One `LATEST ON` for the whole fleet rather than a query per channel.
    let latest = series::fetch_latest(&app.client, base)
        .await
        .unwrap_or_default();
    let status = series::fetch_status(&app.client, &app.settings.questdb.status_table)
        .await
        .unwrap_or_default();

    let mut out: Vec<Channel> = keys
        .into_iter()
        .map(|(node, sensor)| {
            // Memory wins over the database: it is at most a flush interval
            // fresher, and never staler.
            let live = app.shared.live(&node, &sensor);
            let stored = latest
                .iter()
                .find(|(n, s, _, _)| *n == node && *s == sensor)
                .map(|(_, _, v, t)| (*v, *t));
            let (last_value, last_at_ms) = match (live, stored) {
                (Some(l), Some(s)) if s.1 > l.1 => (Some(s.0), Some(s.1)),
                (Some(l), _) => (Some(l.0), Some(l.1)),
                (None, Some(s)) => (Some(s.0), Some(s.1)),
                (None, None) => (None, None),
            };
            Channel {
                meta: app.shared.meta(&node, &sensor).unwrap_or_default(),
                online: app
                    .shared
                    .online(&node)
                    .or_else(|| status.iter().find(|(n, _)| *n == node).map(|(_, o)| *o)),
                node,
                sensor,
                last_value,
                last_at_ms,
            }
        })
        .collect();

    // Stable order, so the sidebar does not reshuffle between polls.
    out.sort_by(|a, b| (&a.node, &a.sensor).cmp(&(&b.node, &b.sensor)));
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
struct SeriesParams {
    node: String,
    sensor: String,
    /// Epoch milliseconds. Defaults to the last 24 hours.
    from: Option<i64>,
    to: Option<i64>,
    /// How many buckets the caller can draw. Bounded so a hand-written URL
    /// cannot ask for a million rows.
    points: Option<i64>,
}

async fn series_handler(
    State(app): State<App>,
    Query(params): Query<SeriesParams>,
) -> ApiResult<Json<series::Series>> {
    let now = now_micros();
    let to: Micros = params.to.map(|ms| ms * 1_000).unwrap_or(now);
    let from: Micros = params
        .from
        .map(|ms| ms * 1_000)
        .unwrap_or(to - 24 * 60 * 60 * 1_000_000);
    if from >= to {
        return Err(ApiError(anyhow::anyhow!(
            "from ({from}) must be before to ({to})"
        )));
    }
    let points = params.points.unwrap_or(600).clamp(2, 5_000);

    let series = series::fetch_series(
        &app.client,
        &app.settings.questdb.table,
        &app.shared.views(),
        &series::Request {
            node: &params.node,
            sensor: &params.sensor,
            from,
            to,
            points,
        },
    )
    .await?;
    Ok(Json(series))
}

#[derive(Debug, Deserialize)]
struct WindowParams {
    from: Option<i64>,
    to: Option<i64>,
    points: Option<i64>,
}

impl WindowParams {
    /// The requested window in microseconds, defaulting to the last day.
    fn window(&self) -> (Micros, Micros) {
        let now = now_micros();
        let to = self.to.map(|ms| ms * 1_000).unwrap_or(now);
        let from = self
            .from
            .map(|ms| ms * 1_000)
            .unwrap_or(to - 24 * 60 * 60 * 1_000_000);
        (from, to)
    }
}

#[derive(Serialize)]
struct OverviewChannel {
    node: String,
    sensor: String,
    #[serde(flatten)]
    meta: crate::model::ChannelMeta,
    last_value: Option<f64>,
    last_at_ms: Option<i64>,
    online: Option<bool>,
    /// `[[t_ms, mean], ...]`, thin enough to draw as a sparkline.
    points: Vec<(i64, f64)>,
}

/// Every channel with its recent shape, for the wall of tiles.
///
/// One database query for the lot -- see `series::overview_sql`. Thirty-one
/// round trips to draw one screen would be the obvious way and the wrong one.
async fn overview(
    State(app): State<App>,
    Query(params): Query<WindowParams>,
) -> ApiResult<Json<Vec<OverviewChannel>>> {
    let base = &app.settings.questdb.table;
    let views = app.shared.views();
    let (from, to) = params.window();
    let points = params.points.unwrap_or(60).clamp(2, 400);

    let shapes = series::fetch_overview(&app.client, base, &views, from, to, points).await?;
    let latest = series::fetch_latest(&app.client, base)
        .await
        .unwrap_or_default();
    let status = series::fetch_status(&app.client, &app.settings.questdb.status_table)
        .await
        .unwrap_or_default();

    let mut keys: Vec<(String, String)> = shapes.iter().map(|(k, _)| k.clone()).collect();
    for (node, sensor, _, _) in &latest {
        let key = (node.clone(), sensor.clone());
        if !keys.contains(&key) {
            keys.push(key);
        }
    }

    let mut out: Vec<OverviewChannel> = keys
        .into_iter()
        .map(|(node, sensor)| {
            let live = app.shared.live(&node, &sensor);
            let stored = latest
                .iter()
                .find(|(n, s, _, _)| *n == node && *s == sensor)
                .map(|(_, _, v, t)| (*v, *t));
            let (last_value, last_at_ms) = match (live, stored) {
                (Some(l), Some(s)) if s.1 > l.1 => (Some(s.0), Some(s.1)),
                (Some(l), _) => (Some(l.0), Some(l.1)),
                (None, Some(s)) => (Some(s.0), Some(s.1)),
                (None, None) => (None, None),
            };
            OverviewChannel {
                meta: app.shared.meta(&node, &sensor).unwrap_or_default(),
                online: app
                    .shared
                    .online(&node)
                    .or_else(|| status.iter().find(|(n, _)| *n == node).map(|(_, o)| *o)),
                points: shapes
                    .iter()
                    .find(|(k, _)| k.0 == node && k.1 == sensor)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default(),
                last_value,
                last_at_ms,
                node,
                sensor,
            }
        })
        .collect();
    out.sort_by(|a, b| (&a.node, &a.sensor).cmp(&(&b.node, &b.sensor)));
    Ok(Json(out))
}

/// The notes that explain the readings, for the window on screen.
async fn annotations_handler(
    State(app): State<App>,
    Query(params): Query<WindowParams>,
) -> ApiResult<Json<Vec<annotations::Annotation>>> {
    let (from, to) = params.window();
    let notes = annotations::fetch(
        &app.client,
        &app.settings.questdb.annotations_table,
        from,
        to,
    )
    .await?;
    Ok(Json(notes))
}

#[derive(Deserialize)]
struct NewAnnotation {
    /// Defaults to now, which is what "something just happened" means.
    at_ms: Option<i64>,
    #[serde(default)]
    node: String,
    note: String,
}

async fn add_annotation(
    State(app): State<App>,
    Json(body): Json<NewAnnotation>,
) -> ApiResult<Json<annotations::Annotation>> {
    let note = annotations::Annotation {
        at_ms: body.at_ms.unwrap_or(now_micros() / 1_000),
        node: body.node,
        note: body.note,
    };
    annotations::insert(&app.client, &app.settings.questdb.annotations_table, &note).await?;
    Ok(Json(note))
}

/// Which note to take back. By instant and node, because that is how a note is
/// addressed everywhere else here -- there is no id, and inventing one would
/// mean rewriting every row already stored.
#[derive(Deserialize)]
struct VoidAnnotation {
    at_ms: i64,
    #[serde(default)]
    node: String,
}

async fn void_annotation(
    State(app): State<App>,
    Json(body): Json<VoidAnnotation>,
) -> ApiResult<Json<Voided>> {
    annotations::void(
        &app.client,
        &app.settings.questdb.annotations_table,
        body.at_ms,
        &body.node,
    )
    .await?;
    Ok(Json(Voided {
        at_ms: body.at_ms,
        node: body.node,
        voided: true,
    }))
}

#[derive(Serialize)]
struct Voided {
    at_ms: i64,
    node: String,
    voided: bool,
}

#[derive(Serialize)]
struct Health {
    broker_connected: bool,
    database_reachable: bool,
    table: String,
    retention: Option<String>,
    views: Vec<String>,
    #[serde(flatten)]
    stats: StatsSnapshot,
}

async fn health(State(app): State<App>) -> Json<Health> {
    let database_reachable = app.client.ping().await.is_ok();
    Json(Health {
        broker_connected: app.shared.broker_connected(),
        database_reachable,
        table: app.settings.questdb.table.clone(),
        retention: app.settings.retention().as_sql().map(|s| s.to_string()),
        views: app.shared.views(),
        stats: app.shared.stats().snapshot(),
    })
}

#[cfg(test)]
mod tests {
    //! The handlers all need a live QuestDB, so what is tested here is the
    //! part that does not: that the assets the binary embeds are the ones the
    //! page actually asks for. A renamed file would otherwise fail at run time
    //! as a blank dashboard.

    #[test]
    fn the_page_references_exactly_the_assets_that_are_served() {
        let html = include_str!("../assets/index.html");
        assert!(html.contains("/style.css"), "stylesheet link missing");
        assert!(html.contains("/app.js"), "script tag missing");
    }

    /// Every route the router declares, for the two tests below. Written out
    /// rather than read off the `Router`, which does not expose its paths.
    const ROUTES: &[&str] = &[
        "/api/channels",
        "/api/series",
        "/api/overview",
        "/api/annotations",
        "/api/annotations/void",
        "/api/health",
    ];

    #[test]
    fn every_endpoint_the_page_calls_exists() {
        // The direction that matters: a typo in a URL is a feature that fails
        // at run time, in the browser, with a 404 nobody is watching for.
        let js = include_str!("../assets/app.js");
        let mut called: Vec<&str> = Vec::new();
        let mut rest = js;
        while let Some(start) = rest.find("/api/") {
            rest = &rest[start..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '/' || c == '_' || c == '-'))
                .unwrap_or(rest.len());
            let path = &rest[..end];
            if !called.contains(&path) {
                called.push(path);
            }
            rest = &rest[end..];
        }
        assert!(!called.is_empty(), "the page calls nothing at all");
        for path in called {
            assert!(ROUTES.contains(&path), "{path} is called but not routed");
        }
    }

    #[test]
    fn the_page_uses_what_the_overview_was_built_for() {
        // `/api/channels` survives for scripts and for debugging, so it is not
        // in this list; these three are what the screen is made of.
        let js = include_str!("../assets/app.js");
        for route in [
            "/api/overview",
            "/api/series",
            "/api/annotations",
            "/api/health",
        ] {
            assert!(js.contains(route), "{route} is never called");
        }
    }

    #[test]
    fn the_page_pulls_nothing_from_the_internet() {
        // The home server's dashboard has to work when the line is down, and
        // a chart library from a CDN is also a third party watching the house.
        let html = include_str!("../assets/index.html");
        assert!(!html.contains("http://"), "{html}");
        assert!(!html.contains("https://"), "{html}");
    }
}
