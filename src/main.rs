#![allow(dead_code)]

use core::fmt;
use std::{
    collections::HashMap,
    future::{Future, IntoFuture},
    io,
    net::Ipv4Addr,
    process::{self, ExitStatus},
    sync::Arc,
};

use ascii::AsciiChar;
use axum::{
    async_trait,
    body::{Body, Bytes, HttpBody},
    extract::{Path, Request, State},
    http::{
        header::{self, CONTENT_TYPE},
        request::Parts,
        HeaderName, StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{method_routing::get, post},
    RequestPartsExt, Router,
};

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use hex::FromHexError;
use indexmap::IndexSet;
use rand::Rng;

use tokio::sync::{Mutex, RwLock};
use tower_http::trace::TraceLayer;

type InternKey = usize;

#[derive(Debug, Default)]
struct ImportTree {
    intern: IndexSet<Utf8PathBuf>,
    imports: HashMap<InternKey, Vec<InternKey>>,
    imported: HashMap<InternKey, Vec<InternKey>>,
}

impl ImportTree {
    fn insert_cydet(
        &mut self,
        root: Utf8PathBuf,
        imports: Utf8PathBuf,
    ) -> Result<(), Vec<Utf8PathBuf>> {
        let root = self.intern.insert_full(root).0;
        let imports = self.intern.insert_full(imports).0;

        let goal = imports;

        let mut stack = vec![vec![root]];

        while let Some(n) = stack.pop() {
            let last = n.last().unwrap();

            if *last == goal {
                return Err(n
                    .into_iter()
                    .map(|v| self.intern.get_index(v).unwrap().clone())
                    .collect());
            }

            if let Some(used) = self.imported.get(last) {
                for i in used {
                    let mut new = n.clone();
                    new.push(*i);

                    stack.push(new);
                }
            }
        }

        self.imports.entry(root).or_default().push(imports);
        self.imported.entry(imports).or_default().push(root);

        Ok(())
    }
}

#[derive(Eq, Hash, PartialEq, Copy, Clone)]
struct HexKey([u8; 16]);

impl HexKey {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).unwrap()
    }

    fn matches(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }

    fn validate(data: &str) -> Result<Self, ()> {
        let Ok(bytes) = <[u8; 16]>::try_from(data.as_bytes()) else {
            return Err(());
        };

        if bytes.iter().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Self(bytes));
        } else {
            return Err(());
        }
    }

    /// generates a new random taskid
    fn new() -> Self {
        let data: [u8; 8] = rand::thread_rng().gen();

        Self(hex::encode(&data).as_bytes().try_into().unwrap())
    }
}

macro_rules! basically_hexkey {
    ($type:ident) => {
        #[derive(Eq, Hash, PartialEq, Copy, Clone)]
        struct $type(HexKey);

        impl $type {
            fn as_str(&self) -> &str {
                self.0.as_str()
            }

            fn matches(&self, other: &str) -> bool {
                self.0.matches(other)
            }

            fn validate(data: &str) -> Result<Self, ()> {
                HexKey::validate(data).map(Self)
            }

            fn new() -> Self {
                Self(HexKey::new())
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.as_str())
            }
        }
    };
}

basically_hexkey!(HurlinKey);
basically_hexkey!(TaskId);

struct ContentTypedBody(Bytes, String);

struct HurlinState {
    key: HurlinKey,
    port: u16,
    import_cache: Mutex<HashMap<Utf8PathBuf, Arc<Mutex<Option<ContentTypedBody>>>>>,
    import_tree: Mutex<ImportTree>,
    running_tasks: RwLock<HashMap<TaskId, Vec<Utf8PathBuf>>>,
    exports: Mutex<HashMap<TaskId, ContentTypedBody>>,
}

impl HurlinState {
    fn new(key: HurlinKey, port: u16) -> Self {
        Self {
            key,
            port,
            import_cache: Default::default(),
            import_tree: Default::default(),
            running_tasks: Default::default(),
            exports: Default::default(),
        }
    }
}

struct HurlVar(String);
struct HurlVarName(String);

impl HurlVarName {
    fn new(s: impl Into<String>) -> Result<Self, String> {
        let s = s.into();

        // ref: https://hurl.dev/docs/grammar.html

        // must be ascii so we can just cheat
        if !s.is_ascii() {
            return Err(s);
        }

        let bytes = s.as_bytes();

        let Some(first) = bytes.get(0) else {
            return Err(s);
        };

        if !(first.is_ascii_lowercase() || first.is_ascii_uppercase()) {
            return Err(s);
        }

        for &b in &bytes[1..] {
            if !(b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err(s);
            }
        }

        Ok(Self(s))
    }
}

struct HurlinRPC(HurlinKey, TaskId, Option<String>);

#[async_trait]
impl axum::extract::FromRequestParts<Arc<HurlinState>> for HurlinRPC {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<HurlinState>,
    ) -> Result<Self, Self::Rejection> {
        let rpc = match parts.extract::<Path<(String, String, String)>>().await {
            Ok(Path((key, task, rpc))) => Self(
                HurlinKey::validate(&key).map_err(|()| StatusCode::BAD_REQUEST)?,
                TaskId::validate(&task).map_err(|()| StatusCode::BAD_REQUEST)?,
                Some(rpc),
            ),
            Err(_) => {
                let Ok(Path((key, task))) = parts.extract::<Path<(String, String)>>().await else {
                    return Err(StatusCode::BAD_REQUEST);
                };

                Self(
                    HurlinKey::validate(&key).map_err(|()| StatusCode::BAD_REQUEST)?,
                    TaskId::validate(&task).map_err(|()| StatusCode::BAD_REQUEST)?,
                    None,
                )
            }
        };

        if rpc.0 != state.key {
            return Err(StatusCode::FORBIDDEN);
        }

        if !state.running_tasks.write().await.contains_key(&rpc.1) {
            return Err(StatusCode::BAD_REQUEST);
        };

        Ok(rpc)
    }
}

fn hurl_call(
    file: Utf8PathBuf,
    variables: impl Iterator<Item = (HurlVarName, HurlVar)>,
) -> impl Future<Output = Result<ExitStatus, io::Error>> {
    let mut p = tokio::process::Command::new("hurl");

    for var in variables {
        p.args(["--variable", format!("{}={}", var.0 .0, var.1 .0).as_ref()]);
    }

    p.arg(file);

    
    println!("{:?}", p.as_std().get_args());

    p.status()
}

fn hurlin_spawn(
    file: Utf8PathBuf,
    variables: impl IntoIterator<Item = (HurlVarName, HurlVar)>,
    key: HurlinKey,
    port: u16,
    task: TaskId,
) -> impl Future<Output = Result<ExitStatus, io::Error>> {
    let vars = [
        (
            HurlVarName::new("hurlin-import").unwrap(),
            HurlVar(format!("http://localhost:{port}/import/{key}/{task}/")),
        ),
        (
            HurlVarName::new("hurlin-export").unwrap(),
            HurlVar(format!("http://localhost:{port}/export/{key}/{task}")),
        ),
    ];

    return hurl_call(file, variables.into_iter().chain(vars));
}

#[axum::debug_handler]
async fn imports(
    HurlinRPC(_, task, path): HurlinRPC,
    State(state): State<Arc<HurlinState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let rlock = state.running_tasks.read().await;

    let Some(ptree) = rlock.get(&task) else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let mut ptree = ptree.clone();
    drop(rlock);

    let last = ptree.last().expect("import chain tree cannot be empty");

    let Some(dir) = last.parent() else {
        eprintln!("Somehow, we were running a directory ({last:?}) and it was the root directory");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    let mut dir = dir.to_path_buf();

    let params = Utf8PathBuf::from(path.unwrap_or(String::new()));
    dir.push(params);

    let Ok(fpath) = dir.canonicalize_utf8() else {
        eprintln!("Failed to canonicalize path: {dir:?}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    if let Err(cycle) = state
        .import_tree
        .lock()
        .await
        .insert_cydet(last.clone(), fpath.clone())
    {
        eprintln!("Cycle detected in import chain:");
        eprintln!("...Attempted to import {fpath}");
        for path in cycle {
            eprintln!("...Which is imported by {path}");
        }
        return Err(StatusCode::FAILED_DEPENDENCY);
    }

    let mut cache = state.import_cache.lock().await;

    let arc = cache.entry(fpath.clone()).or_default().clone();

    let mut file_owner = arc.lock().await;

    drop(cache);

    match &*file_owner {
        Some(ContentTypedBody(data, mime)) => {
            return Ok(([(header::CONTENT_TYPE, mime.clone())], data.clone()).into_response());
        }
        None => {
            let mut tasks = state.running_tasks.write().await;

            let mut task_id = TaskId::new();

            // ensure no collisions
            while tasks.contains_key(&task_id) {
                task_id = TaskId::new();
            }

            ptree.push(fpath.clone());
            tasks.insert(task_id, ptree);

            drop(tasks);

            if !hurlin_spawn(fpath, [], state.key, state.port, task_id)
                .await
                .unwrap().success() {
                eprintln!("Hurl task {task_id} failed to run to completion");
                return Err(StatusCode::FAILED_DEPENDENCY);
            }

            if let Some(res) = state.exports.lock().await.remove(&task_id) {
                let data = file_owner.insert(res);

                return Ok(
                    ([(header::CONTENT_TYPE, data.1.clone())], data.0.clone()).into_response()
                );
            } else {
                return Ok(().into_response());
            }
        }
    };
}

#[axum::debug_handler]
async fn exports(
    HurlinRPC(_, task_id, path): HurlinRPC,
    State(state): State<Arc<HurlinState>>,
    parts: Request,
) -> Result<impl IntoResponse, StatusCode> {

    if let Some(ty) = parts.headers().get(header::CONTENT_TYPE) {
        let Ok(mime) = ty.to_str().map(String::from) else {
            eprintln!("CONTENT_TYPE provided but not ascii");
            return Err(StatusCode::BAD_REQUEST);
        };

        let body = parts.into_body();

        let Ok(bytes) = axum::body::to_bytes(body, 4_000_000).await else {
            eprintln!("Body exceeds 4MB in size");
            return Err(StatusCode::BAD_REQUEST);
        };

        state
            .exports
            .lock()
            .await
            .insert(task_id, ContentTypedBody(bytes, mime));
    }

    Ok(())
}

#[axum::debug_handler]
async fn noise(
    Path((_, key, params)): Path<(String, String, String)>,
    State(state): State<Arc<HurlinState>>,
) -> impl IntoResponse {
    println!("params: {params:?}");
}

#[derive(clap::Parser)]
/// MVP: a bolt on hurl wrapper with enhanced functionality
struct Args {
    /// file to treat as the root node
    file: Utf8PathBuf,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let fuzz_key = HurlinKey::new();

    let addr = tokio::net::TcpListener::bind((Ipv4Addr::from_bits(0), 0))
        .await
        .unwrap();

    let port = addr.local_addr().unwrap().port();

    let state = Arc::new(HurlinState::new(fuzz_key, port));

    let app = Router::new()
        .route("/import/:fuzz_key/:taskid/*params", get(imports))
        .route("/export/:fuzz_key/:taskid", post(exports))
        .route("/noise/:fuzz_key/:taskid/*params", get(noise))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let server = tokio::spawn(axum::serve(addr, app).into_future());

    println!("HurlinKey for this session: {}", fuzz_key);
    println!("Starting local server on port {}", port);

    let root = TaskId::new();

    state
        .running_tasks
        .write()
        .await
        .insert(root, vec![args.file.clone()]);

    hurlin_spawn(args.file, [], fuzz_key, port, root)
        .await
        .unwrap();

    server.abort();
}
