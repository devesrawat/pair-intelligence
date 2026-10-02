//! Test fixtures: a local mock HTTP provider (axum on 127.0.0.1) and per-run Postgres databases.
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use bytes::Bytes;
use futures_util::stream;
use std::convert::Infallible;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Captured {
    pub path: String,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
}

#[derive(Clone)]
pub enum Reply {
    /// Respond with these body chunks (each flushed separately).
    Stream {
        status: u16,
        headers: Vec<(&'static str, String)>,
        chunks: Vec<String>,
    },
    /// Send one chunk, then stall forever; `dropped` flips when the server side body is dropped.
    Hang {
        first: String,
        dropped: Arc<AtomicBool>,
    },
}

pub struct Mock {
    pub base: String,
    pub captured: Arc<Mutex<Vec<Captured>>>,
}

#[derive(Clone)]
struct AppState {
    reply: Reply,
    captured: Arc<Mutex<Vec<Captured>>>,
}

struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn handle(State(st): State<AppState>, req: Request<Body>) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    if let Ok(mut g) = st.captured.lock() {
        g.push(Captured {
            path: parts.uri.path().to_owned(),
            headers: parts.headers,
            body: json,
        });
    }
    match st.reply {
        Reply::Stream {
            status,
            headers,
            chunks,
        } => {
            let items: Vec<Result<Bytes, Infallible>> =
                chunks.into_iter().map(|c| Ok(Bytes::from(c))).collect();
            let mut b =
                Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::OK));
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b.body(Body::from_stream(stream::iter(items)))
                .unwrap_or_default()
        }
        Reply::Hang { first, dropped } => {
            let guard = DropFlag(dropped);
            let s = stream::unfold((guard, false), move |(g, sent)| {
                let first = first.clone();
                async move {
                    if sent {
                        std::future::pending::<()>().await;
                        None
                    } else {
                        Some((Ok::<_, Infallible>(Bytes::from(first)), (g, true)))
                    }
                }
            });
            Response::builder()
                .status(200)
                .body(Body::from_stream(s))
                .unwrap_or_default()
        }
    }
}

pub async fn start(reply: Reply) -> Mock {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new().fallback(handle).with_state(AppState {
        reply,
        captured: captured.clone(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Mock {
        base: format!("http://{addr}"),
        captured,
    }
}

/// A uniquely named database migrated from `migrations/`, dropped on `Drop`.
pub struct TestDb {
    pub url: String,
    name: String,
    admin_url: String,
}

fn db_url(name: &str) -> String {
    let base = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://pair:pair@127.0.0.1:55432/pair".to_owned());
    let mut u = url::Url::parse(&base).expect("DATABASE_URL parses");
    u.set_path(name);
    u.to_string()
}

impl TestDb {
    pub async fn create() -> Self {
        let name = format!("pair_t_providers_{}", Uuid::new_v4().simple());
        let admin_url = db_url("postgres");
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect admin db");
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .expect("create test db");
        admin.close().await;
        let db = Self {
            url: db_url(&name),
            name,
            admin_url,
        };
        let pool = db.pool().await;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        let m = sqlx::migrate::Migrator::new(dir)
            .await
            .expect("load migrations");
        m.run(&pool).await.expect("run migrations");
        pool.close().await;
        db
    }

    pub async fn pool(&self) -> sqlx::PgPool {
        sqlx::PgPool::connect(&self.url)
            .await
            .expect("connect test db")
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let (admin_url, name) = (self.admin_url.clone(), self.name.clone());
        let _ = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async {
                if let Ok(admin) = sqlx::PgPool::connect(&admin_url).await {
                    let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                        .execute(&admin)
                        .await;
                    admin.close().await;
                }
            });
        })
        .join();
    }
}
