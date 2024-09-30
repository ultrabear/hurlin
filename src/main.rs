#![allow(dead_code)]

use core::fmt;
use std::{
    collections::HashMap,
    fmt::Display,
    future::{Future, IntoFuture},
    io::{self, Write},
    net::Ipv4Addr,
    process::{self, ExitCode},
    sync::Arc,
};

use axum::{
    async_trait,
    body::Bytes,
    extract::{Path, Request, State},
    http::{
        header::{self, CONTENT_TYPE},
        request::Parts,
        StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{method_routing::get, post},
    RequestPartsExt, Router,
};

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use indexmap::IndexSet;
use rand::Rng;

use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use tower_http::trace::TraceLayer;

type InternKey = usize;
// make it easier to intern later lolol
type FileNameRef = Utf8PathBuf;

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

impl HurlinKey {
    fn validate_or_log(data: &str) -> Result<Self, StatusCode> {
        Self::validate(data).map_err(|_| {
            tracing::error!("Failed to parse HurlinKey");
            StatusCode::BAD_REQUEST
        })
    }
}

impl TaskId {
    fn validate_or_log(data: &str) -> Result<Self, StatusCode> {
        Self::validate(data).map_err(|_| {
            tracing::error!("Failed to parse TaskId");
            StatusCode::BAD_REQUEST
        })
    }
}

struct TypedBody(Bytes, String);

type UnlockedCacheEntry = Option<Result<Option<TypedBody>, StatusCode>>;
type CacheEntry = Arc<Mutex<UnlockedCacheEntry>>;

struct HurlinState {
    key: HurlinKey,
    port: u16,
    hurl_args: Arc<[String]>,
    hostname: String,
    import_cache: Mutex<HashMap<Utf8PathBuf, CacheEntry>>,
    import_tree: Mutex<ImportTree>,
    running_tasks: RwLock<HashMap<TaskId, Vec<Utf8PathBuf>>>,
    exports: Mutex<HashMap<TaskId, TypedBody>>,
}

impl HurlinState {
    fn new(key: HurlinKey, port: u16, hurl_args: Arc<[String]>, hostname: String) -> Self {
        Self {
            key,
            port,
            hurl_args,
            hostname,
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
                HurlinKey::validate_or_log(&key)?,
                TaskId::validate_or_log(&task)?,
                Some(rpc),
            ),
            Err(_) => {
                let Ok(Path((key, task))) = parts.extract::<Path<(String, String)>>().await else {
                    return Err(StatusCode::BAD_REQUEST);
                };

                Self(
                    HurlinKey::validate_or_log(&key)?,
                    TaskId::validate_or_log(&task)?,
                    None,
                )
            }
        };

        if rpc.0 != state.key {
            tracing::error!("Request sent with invalid HurlinKey");
            return Err(StatusCode::FORBIDDEN);
        }

        if !state.running_tasks.read().await.contains_key(&rpc.1) {
            tracing::error!("Request sent with invalid TaskId");
            return Err(StatusCode::BAD_REQUEST);
        };

        Ok(rpc)
    }
}

fn hurl_call(
    file: Utf8PathBuf,
    variables: impl Iterator<Item = (HurlVarName, HurlVar)>,
    arguments: Arc<[String]>,
) -> impl Future<Output = Result<std::process::Output, io::Error>> {
    let mut p = tokio::process::Command::new("hurl");
    p.args(arguments.into_iter());
    p.arg("--color");

    for var in variables {
        p.args(["--variable", format!("{}={}", var.0 .0, var.1 .0).as_ref()]);
    }

    p.arg(file);

    p.output()
}

fn hurlin_spawn(
    file: Utf8PathBuf,
    variables: impl IntoIterator<Item = (HurlVarName, HurlVar)>,
    args: Arc<[String]>,
    key: HurlinKey,
    port: u16,
    task: TaskId,
) -> impl Future<Output = Result<process::Output, io::Error>> {
    let vars = [
        (
            HurlVarName::new("hurlin-import").unwrap(),
            HurlVar(format!("http://localhost:{port}/import/{key}/{task}/")),
        ),
        (
            HurlVarName::new("hurlin-export").unwrap(),
            HurlVar(format!("http://localhost:{port}/export/{key}/{task}/")),
        ),
        (
            HurlVarName::new("hurlin-noise").unwrap(),
            HurlVar(format!("http://localhost:{port}/noise/{key}/{task}/")),
        ),
    ];

    return hurl_call(file, variables.into_iter().chain(vars), args);
}

struct Hyperlink<H: Display, T: Display> {
    hyperlink: H,
    text: T,
}

impl<H: Display, T: Display> Hyperlink<H, T> {
    fn new(hyperlink: H, text: T) -> Self {
        Self { hyperlink, text }
    }
}

impl<H: Display, T: Display> Display for Hyperlink<H, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\",
            self.hyperlink, self.text
        )
    }
}

struct HighlightFile<'a, Ansi: Display>(&'a Utf8Path, Ansi);

impl<'a, Ansi: Display> Display for HighlightFile<'a, Ansi> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let Some(file) = self.0.file_name() else {
            return Display::fmt(&self.0, f);
        };

        let Some(parent) = self.0.parent() else {
            return Display::fmt(&self.0, f);
        };

        write!(f, "{parent}/\x1b[{}m{file}\x1b[0m", self.1)
    }
}

fn detect_import_cycle(
    itree: &mut ImportTree,
    hostname: &str,
    last: Utf8PathBuf,
    fpath: Utf8PathBuf,
) -> Result<(), StatusCode> {
    if let Err(cycle) = itree.insert_cydet(last.clone(), fpath.clone()) {
        eprintln!("\x1b[93;1mcycle detected in import chain:\x1b[0m");
        eprintln!(
            "\x1b[37mAttempted to import\x1b[0m {}",
            Hyperlink::new(
                format_args!("file://{}{fpath}", hostname),
                HighlightFile(&fpath, 91)
            )
        );

        for (i, path) in cycle.iter().enumerate() {
            let filelink = format!("file://{}{path}", hostname);

            let link: &dyn Display = if i == (cycle.len() - 1) {
                &Hyperlink::new(filelink, HighlightFile(&path, 91)) as _
            } else {
                &Hyperlink::new(filelink, &path) as _
            };

            eprintln!("\x1b[37m...which is imported by\x1b[0m {}", link);
        }
        Err(StatusCode::FAILED_DEPENDENCY)
    } else {
        Ok(())
    }
}

async fn run_hurlin_task(
    fpath: Utf8PathBuf,
    mut ptree: Vec<Utf8PathBuf>,
    mut file_owner: OwnedMutexGuard<UnlockedCacheEntry>,
    state: Arc<HurlinState>,
) -> Result<Response, StatusCode> {
    let mut tasks = state.running_tasks.write().await;

    let mut task_id = TaskId::new();

    // ensure no collisions
    while tasks.contains_key(&task_id) {
        task_id = TaskId::new();
    }

    ptree.push(fpath.clone());
    tasks.insert(task_id, ptree);

    drop(tasks);

    let res = hurlin_spawn(
        fpath.clone(),
        [],
        state.hurl_args.clone(),
        state.key,
        state.port,
        task_id,
    )
    .await
    .unwrap();

    state.running_tasks.write().await.remove(&task_id);

    _ = std::io::stdout().lock().write_all(&res.stdout);
    _ = std::io::stderr().lock().write_all(&res.stderr);

    if !res.status.success() {
        eprintln!("Hurl task {task_id} ({fpath}) failed to run to completion");
        *file_owner = Some(Err(StatusCode::FAILED_DEPENDENCY));
        return Err(StatusCode::FAILED_DEPENDENCY);
    } else {
        if let Some(res) = state.exports.lock().await.remove(&task_id) {
            let data = file_owner.insert(Ok(Some(res)));

            // unwrap safe as we just inserted
            let data = data.as_mut().unwrap().as_mut().unwrap();

            return Ok(([(header::CONTENT_TYPE, data.1.clone())], data.0.clone()).into_response());
        } else {
            *file_owner = Some(Ok(None));
            return Ok(().into_response());
        }
    }
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

    detect_import_cycle(
        &mut *state.import_tree.lock().await,
        &state.hostname,
        last.clone(),
        fpath.clone(),
    )?;

    let mut cache = state.import_cache.lock().await;

    let arc = cache.entry(fpath.clone()).or_default().clone();

    let file_owner = arc.lock_owned().await;

    drop(cache);

    match &*file_owner {
        // cached with no response body/export
        Some(Ok(None)) => Ok(StatusCode::OK.into_response()),
        // cached with a response body
        Some(Ok(Some(TypedBody(data, mime)))) => {
            Ok(([(header::CONTENT_TYPE, mime.clone())], data.clone()).into_response())
        }
        // cached failure to run successfully
        Some(Err(status)) => Err(*status),
        // no cache
        None => run_hurlin_task(fpath, ptree, file_owner, state.clone()).await,
    }
}

#[axum::debug_handler]
async fn hurlin_async_call(
    HurlinRPC(_, task, path): HurlinRPC,
    State(state): State<Arc<HurlinState>>,
) -> Result<impl IntoResponse, StatusCode> {
    Ok(())
}

#[axum::debug_handler]
async fn exports(
    HurlinRPC(_, task_id, _): HurlinRPC,
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
            .insert(task_id, TypedBody(bytes, mime));
    }

    Ok(())
}

async fn noise(_: HurlinRPC) -> impl IntoResponse {
    let data = hex::encode(&rand::thread_rng().gen::<[u8; 8]>());

    let r_data = typed_json::json!({
        "noise": data,
    });

    (
        [(CONTENT_TYPE, "application/json")],
        serde_json::to_string(&r_data).expect("this is always valid json"),
    )
}

#[derive(clap::Parser)]
/// MVP: a bolt on hurl wrapper with enhanced functionality
struct Args {
    /// file to treat as the root node
    file: Utf8PathBuf,

    /// Arguments to forward to hurl verbatim
    #[clap(last = true)]
    hurl: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let hurl_args = Arc::from_iter(args.hurl);

    let hostname = gethostname::gethostname()
        .to_str()
        .map_or_else(String::new, String::from);

    let Ok(root_f) = args.file.canonicalize_utf8() else {
        eprintln!("Failed to cannonicalize input file, init failure");
        return ExitCode::FAILURE;
    };

    let fuzz_key = HurlinKey::new();

    let addr = tokio::net::TcpListener::bind((Ipv4Addr::from_bits(0), 0))
        .await
        .unwrap();

    let port = addr.local_addr().unwrap().port();

    let state = Arc::new(HurlinState::new(
        fuzz_key,
        port,
        hurl_args.clone(),
        hostname,
    ));

    let app = Router::new()
        .route("/import/:fuzz_key/:taskid/*params", get(imports))
        .route("/export/:fuzz_key/:taskid/", post(exports))
        .route("/noise/:fuzz_key/:taskid/", get(noise))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let server = tokio::spawn(axum::serve(addr, app).into_future());

    tracing::debug!("HurlinKey for this session: {}", fuzz_key);
    tracing::debug!("Starting local server on port {}", port);

    let root = TaskId::new();

    state
        .running_tasks
        .write()
        .await
        .insert(root, vec![root_f.clone()]);

    let res = hurlin_spawn(root_f, [], hurl_args, fuzz_key, port, root)
        .await
        .unwrap();

    _ = std::io::stdout().lock().write_all(&res.stdout);
    _ = std::io::stderr().lock().write_all(&res.stderr);

    server.abort();

    if !res.status.success() {
        eprintln!("Root hurlin task failed to run to completion");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
