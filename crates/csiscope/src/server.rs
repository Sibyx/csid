//! HTTP + WebSocket surface.
//!
//! One WebSocket carries the live analysis; a small REST surface carries
//! everything that is not per-frame (configuration, unit control, sessions,
//! diagnostics). The UI is compiled into the binary, so deployment is still
//! "copy one file".
//!
//! ## No authentication, on purpose
//!
//! This is a lab instrument. It has no login, no sessions and no CSRF tokens,
//! and it will happily start and stop capture units for whoever can reach the
//! port. That is a deployment constraint, not an oversight: bind it to
//! localhost or a management VLAN, or run with `--read-only` to serve the views
//! without the write surface. `--bind` defaults to loopback so the unsafe
//! choice is the explicit one.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::analyze::ClientView;
use crate::console;
use crate::frame::ViewSettings;
use crate::pipeline::Pipeline;
use crate::state::Hub;

/// Everything a handler needs.
pub struct App {
    pub hub: Arc<Hub>,
    /// The running analyses, one per distinct view. See [`Pipeline`].
    pub pipeline: Pipeline,
    pub config_path: PathBuf,
    pub experiment_dir: PathBuf,
    /// Refuse every mutating route. The views stay live.
    pub read_only: bool,
    /// The `csid` binary used for `doctor` and `export` — the CLI is the
    /// façade, so the console reports exactly what the operator would see on
    /// the shell.
    pub csid_bin: String,
    /// Default interface for `csid doctor`.
    pub interface: String,
}

pub type Shared = Arc<App>;

/// Build the router.
pub fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(asset_js))
        .route("/plot.js", get(asset_plot))
        .route("/style.css", get(asset_css))
        .route("/ws", get(ws_upgrade))
        .route("/api/overview", get(overview))
        .route("/api/experiments", get(experiments))
        .route("/api/experiments/{name}", get(experiment_get))
        .route("/api/experiments/{name}", put(experiment_put))
        .route("/api/experiments/{name}", delete(experiment_delete))
        .route("/api/experiments/{name}/validate", post(experiment_check))
        .route("/api/config", get(config_get))
        .route("/api/config", put(config_put))
        .route("/api/units", get(units))
        .route("/api/units/{unit}/{action}", post(unit_action))
        .route("/api/journal", get(journal))
        .route("/api/doctor", get(doctor))
        .route("/api/caps", get(caps))
        .route("/api/sessions", get(sessions))
        .route("/api/sessions/{id}/export", post(session_export))
        .with_state(app)
}

/// How long to keep retrying a bind that fails *only* because the address is
/// not on this host yet.
///
/// On the fleet the console binds the node's tailnet address, which tailscaled
/// configures asynchronously during boot. Losing that race is expected and
/// self-correcting; every other bind failure is a misconfiguration that waiting
/// cannot fix, so only this one is retried.
const BIND_WAIT: std::time::Duration = std::time::Duration::from_secs(180);

/// Bind, tolerating an address that has not appeared yet.
///
/// `EADDRNOTAVAIL` is the one bind error that means "not yet" rather than
/// "no". Anything else — the port is taken, the port is privileged — fails
/// immediately, because retrying it would turn a clear error into a three
/// minute silence.
async fn bind_when_available(bind: SocketAddr) -> Result<tokio::net::TcpListener> {
    let deadline = std::time::Instant::now() + BIND_WAIT;
    let mut last_warned: Option<std::time::Instant> = None;

    loop {
        match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => return Ok(listener),
            Err(e)
                if e.kind() == std::io::ErrorKind::AddrNotAvailable
                    && std::time::Instant::now() < deadline =>
            {
                // Say so periodically rather than once: a genuinely wrong
                // --bind looks exactly like a slow tailnet for as long as this
                // loop runs, and the operator should be able to tell from the
                // journal which one they are watching.
                let now = std::time::Instant::now();
                if last_warned.is_none_or(|t| now.duration_since(t).as_secs() >= 15) {
                    tracing::warn!(
                        %bind,
                        "address is not on this host yet — waiting for it to appear"
                    );
                    last_warned = Some(now);
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("binding {bind}")),
        }
    }
}

/// Serve the console.
pub async fn serve(app: Shared, bind: SocketAddr) -> Result<()> {
    let listener = bind_when_available(bind).await?;
    tracing::info!(%bind, read_only = app.read_only, "console listening (no authentication)");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(shutdown())
        .await
        .context("serving HTTP")
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}

// -- static assets ------------------------------------------------------------

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const PLOT_JS: &str = include_str!("../ui/plot.js");
const STYLE_CSS: &str = include_str!("../ui/style.css");

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // The assets are baked into the binary, so a stale cache after an
            // upgrade would be confusing with no way for the operator to know.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}
async fn asset_js() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}
async fn asset_plot() -> Response {
    asset("text/javascript; charset=utf-8", PLOT_JS)
}
async fn asset_css() -> Response {
    asset("text/css; charset=utf-8", STYLE_CSS)
}

// -- the live socket ----------------------------------------------------------

async fn ws_upgrade(ws: WebSocketUpgrade, State(app): State<Shared>) -> Response {
    ws.max_message_size(1 << 20)
        .on_upgrade(move |socket| live(socket, app))
}

/// One client's live session.
///
/// Settings are per-connection, so two browsers can watch different chains at
/// different frame rates off the same capture. Clients whose settings *agree*
/// now share one analysis: the windowed views are identical for both, and only
/// the waterfall — which follows each client's own cursor through the ring —
/// is drawn per connection. Changing a knob moves this client onto a different
/// shared analysis, starting one if it is the first to ask for that view.
async fn live(mut socket: WebSocket, app: Shared) {
    let mut settings = ViewSettings::default();
    settings.sanitise();
    let mut client = ClientView::at_live_edge(&app.hub);
    let mut subscription = app.pipeline.subscribe(&settings);
    let mut ticker = tokio::time::interval(settings.interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Tell the client what it is looking at before the first frame arrives.
    let hello = json!({
        "t": "hello",
        "source": app.hub.source,
        "read_only": app.read_only,
        "csid_version": csid::VERSION,
        "csiscope_version": env!("CARGO_PKG_VERSION"),
        "settings": settings,
    });
    if socket
        .send(Message::Text(hello.to_string().into()))
        .await
        .is_err()
    {
        return;
    }

    loop {
        tokio::select! {
            // A settings update: apply, acknowledge, retime.
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ViewSettings>(&text) {
                            Ok(mut next) => {
                                next.sanitise();
                                let refps = (next.fps - settings.fps).abs() > f32::EPSILON;
                                // Anything that changes the numbers moves this
                                // client to a different shared analysis; a
                                // change of frame rate only retimes it.
                                let review = next.view_key() != settings.view_key();
                                settings = next;
                                if review {
                                    subscription = app.pipeline.subscribe(&settings);
                                }
                                if refps {
                                    subscription.set_fps(settings.fps);
                                    ticker = tokio::time::interval(settings.interval());
                                    ticker.set_missed_tick_behavior(
                                        tokio::time::MissedTickBehavior::Delay,
                                    );
                                }
                                let ack = json!({"t": "settings", "settings": settings});
                                if socket.send(Message::Text(ack.to_string().into())).await.is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                let err = json!({"t": "error", "error": e.to_string()});
                                let _ = socket.send(Message::Text(err.to_string().into())).await;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    // Closed or errored.
                    _ => return,
                }
            }

            _ = ticker.tick() => {
                if settings.paused {
                    continue;
                }
                // The heavy work already happened on the shared task; all this
                // client owes is its own waterfall rows and the splice.
                let Some(shared) = subscription.latest() else {
                    continue;
                };
                let bytes = Vec::from(client.render(&app.hub, &shared));
                if socket.send(Message::Binary(bytes.into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

// -- REST ---------------------------------------------------------------------

/// A handler error rendered as JSON, so the UI can show the daemon's own words.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error": self.1}))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
    }
}

type ApiResult = std::result::Result<Json<serde_json::Value>, ApiError>;

fn writable(app: &App) -> std::result::Result<(), ApiError> {
    if app.read_only {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "csiscope is running with --read-only".into(),
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct TomlBody {
    toml: String,
}

async fn overview(State(app): State<Shared>) -> ApiResult {
    let global = csid::config::GlobalConfig::load(&app.config_path).unwrap_or_default();
    let exps = console::list_experiments(&app.experiment_dir);
    let units: Vec<_> = exps
        .iter()
        .map(|e| console::unit_status(&format!("csid@{}.service", e.name)))
        .collect();

    Ok(Json(json!({
        "csid_version": csid::VERSION,
        "csiscope_version": env!("CARGO_PKG_VERSION"),
        "read_only": app.read_only,
        "source": app.hub.source,
        "config_path": app.config_path.display().to_string(),
        "experiment_dir": app.experiment_dir.display().to_string(),
        "interface": app.interface,
        "hostname": csid::util::run_opt("hostname", &[]),
        "kernel": csid::util::run_opt("uname", &["-r"]),
        "global": global,
        "experiments": exps,
        "units": units,
        "helpers": {
            "csid": std::path::Path::new(&app.csid_bin).exists()
                || which(&app.csid_bin).is_some(),
            "systemctl": which("systemctl").is_some(),
            "journalctl": which("journalctl").is_some(),
        },
    })))
}

/// Is a bare command name on PATH? Used to tell "unit is stopped" apart from
/// "this box has no systemd", which the console shows differently.
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|p| p.join(bin))
        .find(|p| p.is_file())
}

async fn experiments(State(app): State<Shared>) -> ApiResult {
    Ok(Json(json!(console::list_experiments(&app.experiment_dir))))
}

async fn experiment_get(State(app): State<Shared>, Path(name): Path<String>) -> ApiResult {
    let e = console::read_experiment(&app.experiment_dir, &name)?;
    Ok(Json(json!(e)))
}

async fn experiment_check(Json(body): Json<TomlBody>) -> ApiResult {
    Ok(Json(json!(console::check_experiment(&body.toml))))
}

async fn experiment_put(
    State(app): State<Shared>,
    Path(name): Path<String>,
    Json(body): Json<TomlBody>,
) -> ApiResult {
    writable(&app)?;
    let check = console::write_experiment(&app.experiment_dir, &name, &body.toml)?;
    tracing::info!(experiment = name, "experiment written");
    Ok(Json(json!({"written": true, "check": check})))
}

async fn experiment_delete(State(app): State<Shared>, Path(name): Path<String>) -> ApiResult {
    writable(&app)?;
    console::delete_experiment(&app.experiment_dir, &name)?;
    tracing::info!(experiment = name, "experiment deleted");
    Ok(Json(json!({"deleted": true})))
}

async fn config_get(State(app): State<Shared>) -> ApiResult {
    let toml = std::fs::read_to_string(&app.config_path).unwrap_or_default();
    Ok(Json(json!({
        "path": app.config_path.display().to_string(),
        "toml": toml,
        "check": console::check_global(&toml),
    })))
}

async fn config_put(State(app): State<Shared>, Json(body): Json<TomlBody>) -> ApiResult {
    writable(&app)?;
    let check = console::write_global(&app.config_path, &body.toml)?;
    tracing::info!(path = %app.config_path.display(), "node configuration written");
    Ok(Json(json!({"written": true, "check": check})))
}

async fn units(State(app): State<Shared>) -> ApiResult {
    let mut out: Vec<_> = console::list_experiments(&app.experiment_dir)
        .iter()
        .map(|e| console::unit_status(&format!("csid@{}.service", e.name)))
        .collect();
    for u in ["csid-sync.timer", "csid-prune.timer"] {
        out.push(console::unit_status(u));
    }
    Ok(Json(json!(out)))
}

async fn unit_action(
    State(app): State<Shared>,
    Path((unit, action)): Path<(String, String)>,
) -> ApiResult {
    writable(&app)?;
    let run = console::unit_action(&unit, &action)?;
    tracing::info!(unit, action, ok = run.ok, "unit action");
    // The action is reported with the unit's resulting state, because
    // `systemctl start` succeeding says nothing about whether the unit stayed up.
    let resolved = console::resolve_unit(&unit)?;
    Ok(Json(json!({
        "run": run,
        "status": console::unit_status(&resolved),
    })))
}

#[derive(Deserialize)]
struct JournalQuery {
    unit: String,
    #[serde(default = "default_lines")]
    lines: usize,
}

fn default_lines() -> usize {
    200
}

async fn journal(Query(q): Query<JournalQuery>) -> ApiResult {
    Ok(Json(json!({"text": console::journal(&q.unit, q.lines)?})))
}

#[derive(Deserialize)]
struct DoctorQuery {
    interface: Option<String>,
}

async fn doctor(State(app): State<Shared>, Query(q): Query<DoctorQuery>) -> ApiResult {
    let iface = q.interface.unwrap_or_else(|| app.interface.clone());
    console::safe_name(&iface)?;
    let run = console::run(&app.csid_bin, &["doctor", "--interface", &iface]);
    Ok(Json(json!(run)))
}

async fn caps() -> ApiResult {
    // Computed in-process: the envelope is a compiled-in constant, and shelling
    // out for it would only add a way to fail.
    Ok(Json(json!(csid::caps::Envelope::default())))
}

async fn sessions(State(app): State<Shared>) -> ApiResult {
    let global = csid::config::GlobalConfig::load(&app.config_path).unwrap_or_default();
    Ok(Json(json!({
        "spool": global.node.spool.display().to_string(),
        "sessions": console::list_sessions(&global.node.spool, 200),
    })))
}

async fn session_export(State(app): State<Shared>, Path(id): Path<String>) -> ApiResult {
    writable(&app)?;
    let global = csid::config::GlobalConfig::load(&app.config_path).unwrap_or_default();
    let dir = console::session_dir(&global.node.spool, &id)?;
    let run = console::run(&app.csid_bin, &["export", &dir.display().to_string()]);
    Ok(Json(json!(run)))
}
