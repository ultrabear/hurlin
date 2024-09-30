use core::fmt;
use std::{
    collections::HashMap,
    fmt::Display,
    future::{Future, IntoFuture},
    io::{self, Write},
    net::Ipv4Addr,
    process::{self, ExitCode},
    sync::{
        atomic::{self, AtomicU64},
        Arc,
    },
};

use axum::{
    async_trait,
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{
        header::{self, CONTENT_TYPE},
        request::Parts,
        StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{method_routing::get, post},
    RequestPartsExt, Router,
};

use camino::Utf8PathBuf;
use clap::Parser;
use indexmap::IndexSet;
use rand::Rng;

use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};
use tower_http::trace::TraceLayer;

mod ansi;
use ansi::{HighlightFile, Hyperlink};

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
basically_hexkey!(AsyncKey);

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

#[derive(Clone)]
struct TypedBody(Bytes, String);

type CacheEntry = Option<Result<Option<TypedBody>, StatusCode>>;
type Locked<T> = Arc<Mutex<T>>;

struct HurlinState {
    key: HurlinKey,
    port: u16,
    hurl_args: Arc<[String]>,
    hurl_calls: AtomicU64,
    import_cache: Mutex<HashMap<Utf8PathBuf, Locked<CacheEntry>>>,
    import_tree: Mutex<ImportTree>,
    running_tasks: RwLock<HashMap<TaskId, Vec<Utf8PathBuf>>>,
    exports: Mutex<HashMap<TaskId, TypedBody>>,
    async_waits: Mutex<HashMap<(AsyncKey, TaskId), Locked<Option<Response>>>>,
}

impl HurlinState {
    fn new(key: HurlinKey, port: u16, hurl_args: Arc<[String]>) -> Self {
        Self {
            key,
            port,
            hurl_args,
            hurl_calls: AtomicU64::new(0),
            import_cache: Default::default(),
            import_tree: Default::default(),
            running_tasks: Default::default(),
            exports: Default::default(),
            async_waits: Default::default(),
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

struct HurlinRPC(TaskId, Option<String>);

#[async_trait]
impl axum::extract::FromRequestParts<Arc<HurlinState>> for HurlinRPC {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<HurlinState>,
    ) -> Result<Self, Self::Rejection> {
        let (key, rpc) = match parts.extract::<Path<(String, String, String)>>().await {
            Ok(Path((key, task, rpc))) => (
                HurlinKey::validate_or_log(&key)?,
                Self(TaskId::validate_or_log(&task)?, Some(rpc)),
            ),
            Err(_) => {
                let Ok(Path((key, task))) = parts.extract::<Path<(String, String)>>().await else {
                    return Err(StatusCode::BAD_REQUEST);
                };

                (
                    HurlinKey::validate_or_log(&key)?,
                    Self(TaskId::validate_or_log(&task)?, None),
                )
            }
        };

        if key != state.key {
            tracing::error!("Request sent with invalid HurlinKey");
            return Err(StatusCode::FORBIDDEN);
        }

        if !state.running_tasks.read().await.contains_key(&rpc.0) {
            tracing::error!("Request sent with invalid TaskId");
            return Err(StatusCode::BAD_REQUEST);
        };

        Ok(rpc)
    }
}

#[derive(Default, Copy, Clone, Debug)]
enum HurlinImportArgs {
    /// default behaviour, read from cache and run if it doesn't exist,
    /// setting the cache after
    #[default]
    Normal,
    /// always run and insert new value into cache, may contribute to
    /// races in hurlin files, beware!
    Recache,
    /// always run and do not interact with cache at all
    Nocache,
}

impl HurlinImportArgs {
    /// whether behaviour should be to always rerun
    fn rerun(&self) -> bool {
        matches!(self, Self::Recache | Self::Nocache)
    }

    /// whether the run should be cached
    fn cache_run(&self) -> bool {
        matches!(self, Self::Normal | Self::Recache)
    }
}

#[derive(serde_derive::Deserialize)]
struct HurlinImportQuery {
    recache: Option<String>,
    nocache: Option<String>,
}

#[async_trait]
impl<S> axum::extract::FromRequestParts<S> for HurlinImportArgs {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let Ok(Query(query)) = parts.extract::<Query<HurlinImportQuery>>().await else {
            eprintln!("Failed to validate HurlinImportQuery parameters");
            return Err(StatusCode::BAD_REQUEST);
        };

        Ok(if query.nocache.is_some() {
            Self::Nocache
        } else if query.recache.is_some() {
            Self::Recache
        } else {
            Self::Normal
        })
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
    state: Arc<HurlinState>,
    task: TaskId,
) -> impl Future<Output = Result<process::Output, io::Error>> {
    let port = state.port;
    let key = state.key;
    let args = state.hurl_args.clone();

    state.hurl_calls.fetch_add(1, atomic::Ordering::Relaxed);

    let vars = [
        (
            HurlVarName::new("hurlin-import").unwrap(),
            HurlVar(format!("http://localhost:{port}/import/{key}/{task}/")),
        ),
        (
            HurlVarName::new("hurlin-async").unwrap(),
            HurlVar(format!("http://localhost:{port}/async/{key}/{task}/")),
        ),
        (
            HurlVarName::new("hurlin-await").unwrap(),
            HurlVar(format!("http://localhost:{port}/await/{key}/{task}/")),
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

fn detect_import_cycle(
    itree: &mut ImportTree,
    parent: FileNameRef,
    imported: FileNameRef,
) -> Result<(), StatusCode> {
    if let Err(cycle) = itree.insert_cydet(parent, imported.clone()) {
        eprintln!("\x1b[93;1mcycle detected in import chain:\x1b[0m");
        eprintln!(
            "\x1b[37mAttempted to import\x1b[0m {}",
            Hyperlink::from_path_named(&imported, HighlightFile(&imported, 91))
        );

        for (i, path) in cycle.iter().enumerate() {
            let link: &dyn Display = if i == (cycle.len() - 1) {
                &Hyperlink::from_path_named(path, HighlightFile(&path, 91)) as _
            } else {
                &Hyperlink::from_path(path) as _
            };

            eprintln!("\x1b[37m...which is imported by\x1b[0m {}", link);
        }
        Err(StatusCode::FAILED_DEPENDENCY)
    } else {
        Ok(())
    }
}

async fn run_hurlin_task(
    file_path: FileNameRef,
    mut call_stack: Vec<FileNameRef>,
    cache: Option<OwnedMutexGuard<CacheEntry>>,
    state: Arc<HurlinState>,
) -> Result<Response, StatusCode> {
    let mut tasks = state.running_tasks.write().await;

    let mut task_id = TaskId::new();

    // ensure no collisions
    while tasks.contains_key(&task_id) {
        task_id = TaskId::new();
    }

    call_stack.push(file_path.clone());
    tasks.insert(task_id, call_stack);

    drop(tasks);

    let res = hurlin_spawn(file_path.clone(), [], state.clone(), task_id)
        .await
        .unwrap();

    state.running_tasks.write().await.remove(&task_id);

    _ = std::io::stdout().lock().write_all(&res.stdout);
    _ = std::io::stderr().lock().write_all(&res.stderr);

    if !res.status.success() {
        eprintln!("Hurl task {task_id} ({file_path}) failed to run to completion");
        if let Some(mut cache_this) = cache {
            *cache_this = Some(Err(StatusCode::FAILED_DEPENDENCY));
        }
        return Err(StatusCode::FAILED_DEPENDENCY);
    } else {
        if let Some(data) = state.exports.lock().await.remove(&task_id) {
            if let Some(mut cache_this) = cache {
                *cache_this = Some(Ok(Some(data.clone())));
            }

            return Ok(([(header::CONTENT_TYPE, data.1)], data.0).into_response());
        } else {
            if let Some(mut cache_this) = cache {
                *cache_this = Some(Ok(None));
            }

            return Ok(().into_response());
        }
    }
}

async fn run_cacheable_hurlin_task(
    state: Arc<HurlinState>,
    file_path: FileNameRef,
    call_stack: Vec<FileNameRef>,
    cache_args: HurlinImportArgs,
    file_owner: OwnedMutexGuard<CacheEntry>,
) -> Result<Response, StatusCode> {
    if cache_args.rerun() {
        // drop early if not needed
        let file_owner = cache_args.cache_run().then_some(file_owner);
        run_hurlin_task(file_path, call_stack, file_owner, state.clone()).await
    } else {
        match &*file_owner {
            // cached with no response body/export
            Some(Ok(None)) => Ok(StatusCode::OK.into_response()),
            // cached with a response body
            Some(Ok(Some(TypedBody(data, mime)))) => {
                Ok(([(header::CONTENT_TYPE, mime.clone())], data.clone()).into_response())
            }
            // cached failure to run successfully
            Some(Err(status)) => Err(*status),
            // not cached
            None => {
                run_hurlin_task(
                    file_path,
                    call_stack,
                    cache_args.cache_run().then_some(file_owner),
                    state.clone(),
                )
                .await
            }
        }
    }
}

struct CallstackInfo {
    call_stack: Vec<FileNameRef>,
    task_name: FileNameRef,
    imports: FileNameRef,
}

async fn callstack_info(
    task_id: TaskId,
    state: &HurlinState,
    request: Option<String>,
) -> Result<CallstackInfo, StatusCode> {
    let rlock = state.running_tasks.read().await;

    let Some(ptree) = rlock.get(&task_id) else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let ptree = ptree.clone();
    drop(rlock);

    let last = ptree
        .last()
        .expect("import chain tree cannot be empty")
        .clone();

    let Some(dir) = last.parent() else {
        eprintln!("Somehow, we were running a directory ({last:?}) and it was the root directory");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    let mut dir = dir.to_path_buf();

    let params = Utf8PathBuf::from(request.unwrap_or(String::new()));
    dir.push(params);

    let Ok(fpath) = dir.canonicalize_utf8() else {
        eprintln!(
            "Failed to canonicalize path: {}, its possible it doesn't exist",
            Hyperlink::from_path(dir)
        );
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };

    Ok(CallstackInfo {
        call_stack: ptree,
        task_name: last,
        imports: fpath,
    })
}

#[axum::debug_handler]
async fn imports(
    HurlinRPC(task, path): HurlinRPC,
    cache_args: HurlinImportArgs,
    State(state): State<Arc<HurlinState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let CallstackInfo {
        call_stack,
        task_name,
        imports,
    } = callstack_info(task, &state, path).await?;

    detect_import_cycle(
        &mut *state.import_tree.lock().await,
        task_name.clone(),
        imports.clone(),
    )?;
    let mut cache = state.import_cache.lock().await;

    let arc = cache.entry(imports.clone()).or_default().clone();

    let file_owner = arc.lock_owned().await;

    drop(cache);

    run_cacheable_hurlin_task(state, imports, call_stack, cache_args, file_owner).await
}

#[axum::debug_handler]
async fn hurlin_async_call(
    HurlinRPC(task, path): HurlinRPC,
    cache_args: HurlinImportArgs,
    State(state): State<Arc<HurlinState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let CallstackInfo {
        call_stack,
        task_name,
        imports,
    } = callstack_info(task, &state, path).await?;

    detect_import_cycle(
        &mut *state.import_tree.lock().await,
        task_name.clone(),
        imports.clone(),
    )?;

    let mut async_map = state.async_waits.lock().await;

    let mut key = AsyncKey::new();

    while async_map.contains_key(&(key, task)) {
        key = AsyncKey::new();
    }

    let arc = async_map.entry((key, task)).or_default().clone();

    let mut async_key_data = arc.lock_owned().await;

    drop(async_map);

    let mut cache = state.import_cache.lock().await;

    let arc = cache.entry(imports.clone()).or_default().clone();

    let file_owner = arc.lock_owned().await;

    drop(cache);

    tokio::spawn(async move {
        let res =
            run_cacheable_hurlin_task(state, imports, call_stack, cache_args, file_owner).await;

        *async_key_data = Some(res.into_response());
    });

    let r_data = typed_json::json!({
        "await": key.to_string(),
    });

    Ok((
        [(CONTENT_TYPE, "application/json")],
        serde_json::to_string(&r_data).expect("this is always valid json"),
    )
        .into_response())
}

#[axum::debug_handler]
async fn hurlin_await(
    HurlinRPC(task, await_key): HurlinRPC,
    State(state): State<Arc<HurlinState>>,
) -> Result<Response, StatusCode> {
    let Ok(key) = AsyncKey::validate(await_key.as_ref().map_or("", String::as_str)) else {
        eprintln!("Could not parse AsyncKey");
        return Err(StatusCode::BAD_REQUEST);
    };

    match state.async_waits.lock().await.remove(&(key, task)) {
        Some(waitable) => Ok(waitable.lock().await.take().unwrap()),
        None => {
            eprintln!(
                "AsyncKey did not exist in wait stack, perhaps something else already claimed it?"
            );
            Err(StatusCode::BAD_REQUEST)
        }
    }
}

#[axum::debug_handler]
async fn exports(
    HurlinRPC(task_id, _): HurlinRPC,
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
#[clap(version)]
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

    let Ok(root_f) = args.file.canonicalize_utf8() else {
        eprintln!("Failed to cannonicalize input file, init failure");
        return ExitCode::FAILURE;
    };

    let fuzz_key = HurlinKey::new();

    let addr = tokio::net::TcpListener::bind((Ipv4Addr::from_bits(0), 0))
        .await
        .unwrap();

    let port = addr.local_addr().unwrap().port();

    let state = Arc::new(HurlinState::new(fuzz_key, port, hurl_args.clone()));

    let app = Router::new()
        .route("/import/:fuzz_key/:taskid/*params", get(imports))
        .route("/async/:fuzz_key/:taskid/*params", get(hurlin_async_call))
        .route("/await/:fuzz_key/:taskid/:param", get(hurlin_await))
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

    let res = hurlin_spawn(root_f, [], state.clone(), root).await.unwrap();

    _ = std::io::stdout().lock().write_all(&res.stdout);
    _ = std::io::stderr().lock().write_all(&res.stderr);

    server.abort();

    tracing::debug!(
        "Hurl was spawned {} time(s)",
        state.hurl_calls.load(atomic::Ordering::Relaxed)
    );

    if !res.status.success() {
        eprintln!("Root hurlin task failed to run to completion");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
