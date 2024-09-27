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
    body::{Body, Bytes, HttpBody},
    extract::{Path, Request, State},
    http::{header, HeaderName, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::method_routing::get,
    Router,
};

use camino::{Utf8Path, Utf8PathBuf};
use hex::FromHexError;
use indexmap::IndexSet;
use rand::Rng;

use tokio::sync::{Mutex, RwLock};

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
    import_cache: Mutex<HashMap<Utf8PathBuf, Arc<Mutex<Option<ContentTypedBody>>>>>,
    import_tree: Mutex<ImportTree>,
    running_tasks: RwLock<HashMap<TaskId, Vec<Utf8PathBuf>>>,
    exports: Mutex<HashMap<TaskId, ContentTypedBody>>,
}

impl HurlinState {
    fn new(key: HurlinKey) -> Self {
        Self {
            key,
            import_cache: Default::default(),
            import_tree: Default::default(),
            running_tasks: Default::default(),
            exports: Default::default(),
        }
    }
}

#[axum::debug_middleware]
async fn fuzz_assert(
    Path((fuzz_key, _, _)): Path<(String, String, String)>,
    State(state): State<Arc<HurlinState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.key.0.matches(&fuzz_key) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::FORBIDDEN)
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

fn hurl_call(
    file: Utf8PathBuf,
    variables: impl Iterator<Item = (HurlVarName, HurlVar)>,
) -> impl Future<Output = Result<ExitStatus, io::Error>> {
    let mut p = tokio::process::Command::new("hurl");

    for var in variables {
        p.args(["--variable", format!("{}={}", var.0 .0, var.1 .0).as_ref()]);
    }

    p.arg(file);

    p.status()
}

async fn hurlin_spawn(
    file: Utf8PathBuf,
    variables: impl IntoIterator<Item = (HurlVarName, HurlVar)>,
    key: HurlinKey,
    task: TaskId,
) {
    let vars = [(
        HurlVarName::new("hurlin-import").unwrap(),
        HurlVar(format!("/import/{key}/{task}/")),
    )];
}

#[axum::debug_handler]
async fn imports(
    Path((_, key, params)): Path<(String, String, Utf8PathBuf)>,
    State(state): State<Arc<HurlinState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let Ok(task) = TaskId::validate(&key) else {
        eprintln!("Request unauthorized: unparsable task_id: {key:?}");
        return Err(StatusCode::UNAUTHORIZED);
    };

    let rlock = state.running_tasks.read().await;

    let Some(ptree) = rlock.get(&task) else {
        eprintln!("Request unathorized: task {key:?} does not exist in running pool");
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

    let file_owner = arc.lock().await;

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

            ptree.push(fpath);
            tasks.insert(task_id, ptree);

            drop(tasks);

            tokio::spawn(async {});
        }
    };

    Ok(().into_response())
}

#[axum::debug_handler]
async fn exports(
    Path((_, key, params)): Path<(String, String, String)>,
    State(state): State<Arc<HurlinState>>,
) -> impl IntoResponse {
    println!("params: {params:?}");
}

#[axum::debug_handler]
async fn noise(
    Path((_, key, params)): Path<(String, String, String)>,
    State(state): State<Arc<HurlinState>>,
) -> impl IntoResponse {
    println!("params: {params:?}");
}

#[tokio::main]
async fn main() {
    let mut fuzz_key = HurlinKey::new();

    println!("{}", fuzz_key);

    let state = Arc::new(HurlinState::new(fuzz_key));

    let app = Router::new()
        .route("/import/:fuzz_key/:taskid/*params", get(imports))
        .route("/export/:fuzz_key/:taskid/*params", get(exports))
        .route("/noise/:fuzz_key/:taskid/*params", get(noise))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            fuzz_assert,
        ))
        .with_state(state);

    let port = tokio::net::TcpListener::bind((Ipv4Addr::from_bits(0), 0))
        .await
        .unwrap();

    println!("{}", port.local_addr().unwrap().port());

    let server = tokio::spawn(axum::serve(port, app).into_future());

    process::Command::new("fish").status();

    println!("exiting hurlin");

    server.abort();
}
