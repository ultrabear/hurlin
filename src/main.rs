#![allow(dead_code)]

use std::{
    collections::HashMap,
    future::{Future, IntoFuture},
    io,
    net::Ipv4Addr,
    process::{self, ExitStatus},
    sync::Arc,
};

use axum::{
    body::Bytes,
    extract::{Path, Request, State},
    http::StatusCode,
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

        self.imports.entry(root).or_default().push(imports);
        self.imported.entry(imports).or_default().push(root);

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

        Ok(())
    }
}

#[derive(Eq, Hash, PartialEq)]
struct TaskId([u8; 8]);

impl TaskId {
    fn to_string(&self) -> String {
        hex::encode(&self.0)
    }

    fn from_string(s: &str) -> Result<Self, ()> {
        if let Ok(data) = hex::decode(s) {
            Ok(Self(data.as_slice().try_into().map_err(|_| ())?))
        } else {
            Err(())
        }
    }

    /// generates a new random taskid
    fn new() -> Self {
        Self(rand::thread_rng().gen())
    }
}

struct HurlinState {
    key: [u8; 16],
    import_cache: Mutex<HashMap<Utf8PathBuf, Arc<Mutex<Option<(Bytes, String)>>>>>,
    import_tree: Mutex<ImportTree>,
    running_tasks: RwLock<HashMap<TaskId, Vec<Utf8PathBuf>>>,
}

impl HurlinState {
    fn new(key: [u8; 16]) -> Self {
        Self {
            key,
            import_cache: Default::default(),
            import_tree: Default::default(),
            running_tasks: Default::default(),
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
    if fuzz_key.as_bytes() == state.key {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

struct HurlVar(String);
struct HurlVarName(String);

impl HurlVarName {
    fn new(s: String) -> Result<Self, String> {
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
    variables: &[(HurlVarName, HurlVar)],
) -> impl Future<Output = Result<ExitStatus, io::Error>> {
    let mut p = tokio::process::Command::new("hurl");

    for var in variables {
        p.args(["--variable", format!("{}={}", var.0 .0, var.1 .0).as_ref()]);
    }

    p.arg(file);

    p.status()
}

#[axum::debug_handler]
async fn imports(
    Path((_, key, params)): Path<(String, String, Utf8PathBuf)>,
    State(state): State<Arc<HurlinState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let Ok(task) = TaskId::from_string(&key) else {
        eprintln!("Request unauthorized: unparsable task_id: {key:?}");
        return Err(StatusCode::UNAUTHORIZED);
    };

    let rlock = state.running_tasks.read().await;

    let Some(ptree) = rlock.get(&task) else {
        eprintln!("Request unathorized: task {key:?} does not exist in running pool");
        return Err(StatusCode::UNAUTHORIZED);
    };

    let ptree = ptree.clone();
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

    if let Err(cycle) = state.import_tree.lock().await.insert_cydet(last.clone(), fpath) {



    }

    Ok(())
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
    let fuzz_raw: [u8; 8] = rand::thread_rng().gen();
    let mut fuzz_key = [0; 16];

    hex::encode_to_slice(fuzz_raw, &mut fuzz_key).unwrap();

    println!("{}", core::str::from_utf8(&fuzz_key).unwrap());

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
