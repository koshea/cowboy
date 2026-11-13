//! The container proxy's rules, as pure functions over the request.
//!
//! Everything that decides whether a Docker API request may reach the engine lives
//! here, away from sockets, so each rule can be tested against the request an agent
//! would actually send. The async layer (`proxy`) only reads the request, asks this
//! module, checks ownership with the engine, and forwards.
//!
//! The request is the agent's, so nothing in it is trusted: unknown fields that could
//! carry privilege are refused rather than ignored, paths are resolved through
//! symlinks before they are judged, and a refusal names the field so the agent (and
//! the user reading the transcript) can see what was asked for.

use std::path::{Component, Path, PathBuf};

use serde_json::{json, Map, Value};

/// The label every object the proxy creates carries, set to the session id. The
/// engine is shared with the user's own containers, and this is how the proxy tells
/// the session's from theirs.
pub const LABEL: &str = "cowboy.session";

/// What this session may do, fixed when the proxy starts.
#[derive(Debug, Clone)]
pub struct Scope {
    pub session: String,
    /// `(path inside the sandbox, path on the host)` for the directories a bind may
    /// come from: the project and the session scratch.
    pub paths: Vec<(PathBuf, PathBuf)>,
    /// Host paths no bind may reach even inside an allowed directory (`.cowboy/`,
    /// which holds the host-owned config).
    pub denied: Vec<PathBuf>,
    pub registries: Vec<String>,
    pub build: bool,
}

/// What the proxy does with one request, from its method and path alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Harmless whoever asks: version, info, image listings.
    Pass,
    /// A listing or stream that must only show this session's objects: forward with
    /// the session label added to `filters`.
    Scoped,
    /// Addressed to a container; forward only if it is this session's.
    Container(String),
    /// A container action after which its published ports may change.
    ContainerLifecycle(String),
    /// `PUT`/`GET` on a container's archive, `wait`, `logs`, `attach`, … (no body
    /// rewriting, streamed).
    Exec(String),
    Network(String),
    Volume(String),
    CreateContainer,
    CreateExec(String),
    CreateNetwork,
    CreateVolume,
    /// `POST /networks/{id}/connect|disconnect`: the network and the container in the
    /// body must both be this session's.
    NetworkConnect(String),
    Pull,
    Build,
    /// `POST /commit?container=…`: a local image of one of this session's containers.
    Commit,
    Deny(String),
}

/// Strip an API version prefix (`/v1.41/containers/json` → `/containers/json`).
pub fn unversioned(path: &str) -> &str {
    let rest = path.strip_prefix("/v").unwrap_or("");
    match rest.find('/') {
        Some(i) if rest[..i].chars().all(|c| c.is_ascii_digit() || c == '.') && i > 0 => &rest[i..],
        _ => path,
    }
}

/// Route a request by method and path (without the query string).
pub fn route(method: &str, path: &str, build: bool) -> Route {
    let path = unversioned(path);
    if path.split('/').any(|s| s == "libpod") {
        return Route::Deny("the libpod API is not available; use the Docker API".into());
    }
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    let id = |i: usize| seg.get(i).map(|s| percent_decode(s)).unwrap_or_default();
    match (method, seg.as_slice()) {
        ("GET" | "HEAD", ["_ping"]) | ("GET", ["version"] | ["info"] | ["system", "df"]) => {
            Route::Pass
        }
        ("GET", ["images", "json"]) => Route::Pass,
        ("GET", ["images", .., "json"] | ["images", .., "history"]) => Route::Pass,
        ("GET", ["events"]) | ("GET", ["containers", "json"]) => Route::Scoped,
        ("GET", ["networks"]) | ("GET", ["volumes"]) => Route::Scoped,
        ("POST", ["containers" | "networks" | "volumes", "prune"]) => Route::Scoped,
        ("POST", ["containers", "create"]) => Route::CreateContainer,
        ("POST", ["containers", _, "exec"]) => Route::CreateExec(id(1)),
        ("POST", ["containers", _, "start" | "stop" | "restart" | "kill"]) => {
            Route::ContainerLifecycle(id(1))
        }
        ("DELETE", ["containers", _]) => Route::ContainerLifecycle(id(1)),
        ("POST", ["containers", _, "update"]) => {
            Route::Deny("updating a container is not available (it can change limits)".into())
        }
        (
            "GET" | "HEAD" | "PUT" | "POST",
            ["containers", _, "json" | "logs" | "top" | "stats" | "changes" | "archive" | "export" | "wait"
            | "attach" | "resize" | "pause" | "unpause" | "rename"],
        ) => Route::Container(id(1)),
        ("GET", ["containers", _, "attach", "ws"]) => Route::Container(id(1)),
        ("POST", ["exec", _, "start" | "resize"]) | ("GET", ["exec", _, "json"]) => {
            Route::Exec(id(1))
        }
        ("POST", ["networks", "create"]) => Route::CreateNetwork,
        ("GET" | "DELETE", ["networks", _]) => Route::Network(id(1)),
        ("POST", ["networks", _, "connect" | "disconnect"]) => Route::NetworkConnect(id(1)),
        ("POST", ["volumes", "create"]) => Route::CreateVolume,
        ("GET" | "DELETE", ["volumes", _]) => Route::Volume(id(1)),
        ("POST", ["images", "create"]) => Route::Pull,
        ("POST", ["images", .., "tag"]) => Route::Pass,
        ("POST", ["images", .., "push"]) => Route::Deny(
            "pushing images is not available: a push is a way to send the project out".into(),
        ),
        ("DELETE", ["images", ..]) | ("POST", ["images", "prune"]) => Route::Deny(
            "removing images is not available: the image store is shared with your own".into(),
        ),
        ("POST", ["build"]) if build => Route::Build,
        ("POST", ["build"]) => Route::Deny(
            "building images is off (sandbox.containers.build in .cowboy/security.yaml)".into(),
        ),
        ("POST", ["commit"]) => Route::Commit,
        _ => Route::Deny(format!(
            "{method} {path} is not available through cowboy's container proxy"
        )),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Add `label=cowboy.session=<id>` to a query string's `filters`, keeping the
/// caller's own (the engine ANDs label filters, so this only narrows).
pub fn scope_query(query: &str, session: &str) -> String {
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let mut filters: Map<String, Value> = pairs
        .iter()
        .find(|(k, _)| k == "filters")
        .and_then(|(_, v)| serde_json::from_str(v).ok())
        .and_then(|v: Value| v.as_object().cloned())
        .unwrap_or_default();
    let ours = format!("{LABEL}={session}");
    // Docker accepts both `{"label": ["a"]}` and `{"label": {"a": true}}`.
    match filters.get_mut("label") {
        Some(Value::Array(a)) => a.push(json!(ours)),
        Some(Value::Object(o)) => {
            o.insert(ours, json!(true));
        }
        _ => {
            filters.insert("label".into(), json!([ours]));
        }
    }
    pairs.retain(|(k, _)| k != "filters");
    pairs.push(("filters".into(), Value::Object(filters).to_string()));
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Objects a create request refers to, which the proxy must confirm are this
/// session's (or create, for a volume) before forwarding.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Needs {
    pub volumes: Vec<String>,
    pub networks: Vec<String>,
    pub containers: Vec<String>,
    /// The request wants the session's own default network.
    pub default_network: bool,
}

/// HostConfig fields allowed with any value. Everything else must be absent or at
/// its zero value, so a field Docker adds tomorrow is refused until reviewed.
const HOST_CONFIG_FREE: &[&str] = &[
    "Binds",
    "Mounts",
    "NetworkMode",
    "PortBindings",
    "PublishAllPorts",
    "RestartPolicy",
    "AutoRemove",
    "VolumesFrom",
    "Links",
    "Dns",
    "DnsOptions",
    "DnsSearch",
    "ExtraHosts",
    "Init",
    "ShmSize",
    "Tmpfs",
    "Ulimits",
    "LogConfig",
    "Sysctls",
    "IpcMode",
    "CgroupnsMode",
    "SecurityOpt",
    "CapDrop",
    "ReadonlyRootfs",
    "GroupAdd",
    "OomScoreAdj",
    "ConsoleSize",
    "Isolation",
    "StorageOpt",
    "Annotations",
    "OomKillDisable",
    "Memory",
    "MemoryReservation",
    "MemorySwap",
    "MemorySwappiness",
    "NanoCpus",
    "CpuShares",
    "CpuPeriod",
    "CpuQuota",
    "CpuRealtimePeriod",
    "CpuRealtimeRuntime",
    "CpusetCpus",
    "CpusetMems",
    "CpuCount",
    "CpuPercent",
    "BlkioWeight",
    "BlkioWeightDevice",
    "BlkioDeviceReadBps",
    "BlkioDeviceWriteBps",
    "BlkioDeviceReadIOps",
    "BlkioDeviceWriteIOps",
    "KernelMemory",
    "KernelMemoryTCP",
    "PidsLimit",
    "IOMaximumIOps",
    "IOMaximumBandwidth",
    "ContainerIDFile",
];

/// HostConfig fields whose presence with a non-zero value is privilege.
const HOST_CONFIG_REFUSED: &[(&str, &str)] = &[
    ("Privileged", "privileged mode"),
    ("CapAdd", "added capabilities"),
    ("Devices", "host devices"),
    ("DeviceRequests", "host devices (GPUs)"),
    ("DeviceCgroupRules", "device cgroup rules"),
    ("PidMode", "a shared PID namespace"),
    ("UTSMode", "the host's UTS namespace"),
    ("UsernsMode", "the host's user namespace"),
    ("Cgroup", "a shared cgroup"),
    ("CgroupParent", "a cgroup parent"),
    ("Runtime", "a different runtime"),
    ("MaskedPaths", "unmasking kernel paths"),
    ("ReadonlyPaths", "unmasking kernel paths"),
    ("VolumeDriver", "a volume driver"),
];

fn is_zero(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn refuse<T>(msg: impl Into<String>) -> Result<T, String> {
    Err(msg.into())
}

/// Resolve `path` through symlinks for a bind that may not exist yet: the deepest
/// existing ancestor is canonicalized and the rest appended, so a missing leaf under
/// a symlink to `~/.ssh` still resolves under `~/.ssh`.
pub fn resolve_host(path: &Path, canon: &dyn Fn(&Path) -> Option<PathBuf>) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        if let Some(c) = canon(&cur) {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return out;
        }
        match (cur.file_name().map(|f| f.to_os_string()), cur.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                cur = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// Translate a bind source the agent named (a path **inside the sandbox**) to the
/// host path the engine must mount, or refuse it.
pub fn translate_bind(
    source: &str,
    scope: &Scope,
    canon: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    let p = Path::new(source);
    if !p.is_absolute() {
        return Err(format!("bind source {source} must be an absolute path"));
    }
    if p.components().any(|c| c == Component::ParentDir) {
        return Err(format!("bind source {source} may not contain `..`"));
    }
    let Some((inside, host)) = scope
        .paths
        .iter()
        .filter(|(inside, _)| p.starts_with(inside))
        .max_by_key(|(inside, _)| inside.as_os_str().len())
    else {
        return Err(format!(
            "bind source {source} is outside the project and the session's temporary \
             directories; containers may only mount those"
        ));
    };
    let rest = p.strip_prefix(inside).unwrap_or(Path::new(""));
    let wanted = if rest.as_os_str().is_empty() {
        host.clone()
    } else {
        host.join(rest)
    };
    let real = resolve_host(&wanted, canon);
    let real_root = canon(host).unwrap_or_else(|| host.clone());
    if !real.starts_with(&real_root) {
        return Err(format!(
            "bind source {source} resolves to {}, outside the directory it is in",
            real.display()
        ));
    }
    if let Some(d) = scope
        .denied
        .iter()
        .find(|d| real.starts_with(resolve_host(d, canon)))
    {
        return Err(format!(
            "bind source {source} is inside {}, which containers may not mount",
            d.display()
        ));
    }
    Ok(real)
}

/// A bind string's parts: `src:dst[:opts]`.
fn split_bind(b: &str) -> Option<(&str, &str, Option<&str>)> {
    let mut it = b.splitn(3, ':');
    let src = it.next()?;
    let dst = it.next()?;
    Some((src, dst, it.next()))
}

/// Check and rewrite a `POST /containers/create` body.
///
/// `image_ports` are the ports the image exposes, which `PublishAllPorts` would
/// publish on every host address: they are bound to `127.0.0.1` explicitly instead.
pub fn rewrite_container_create(
    body: &mut Value,
    scope: &Scope,
    image_ports: &[String],
    canon: &dyn Fn(&Path) -> Option<PathBuf>,
) -> Result<Needs, String> {
    let mut needs = Needs::default();
    let obj = body
        .as_object_mut()
        .ok_or("the container definition is not a JSON object")?;

    // The label that makes it this session's, whatever the request said.
    let labels = obj.entry("Labels").or_insert_with(|| json!({}));
    if labels.is_null() {
        *labels = json!({});
    }
    labels
        .as_object_mut()
        .ok_or("Labels is not an object")?
        .insert(LABEL.into(), json!(scope.session));

    let exposed: Vec<String> = obj
        .get("ExposedPorts")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    let hc = obj.entry("HostConfig").or_insert_with(|| json!({}));
    if hc.is_null() {
        *hc = json!({});
    }
    let hc = hc.as_object_mut().ok_or("HostConfig is not an object")?;

    for (key, what) in HOST_CONFIG_REFUSED {
        if let Some(v) = hc.get(*key) {
            // An empty mode, a private cgroup namespace: the defaults, not a grant.
            // An empty list of masked paths is not a default: it unmasks them all.
            let unmasks = matches!(*key, "MaskedPaths" | "ReadonlyPaths") && !v.is_null();
            let harmless = (!unmasks && is_zero(v))
                || (*key == "PidMode" && v == "private")
                || (*key == "UTSMode" && v == "private")
                || (*key == "UsernsMode" && v == "private");
            if !harmless {
                return Err(format!(
                    "{what} ({key}) is not available to containers here"
                ));
            }
        }
    }
    for (key, v) in hc.iter() {
        if !is_zero(v)
            && !HOST_CONFIG_FREE.contains(&key.as_str())
            && !HOST_CONFIG_REFUSED.iter().any(|(k, _)| k == key)
        {
            return Err(format!(
                "HostConfig.{key} is not recognised by cowboy's container proxy, so it is \
                 refused"
            ));
        }
    }

    if let Some(Value::String(ipc)) = hc.get("IpcMode") {
        if !matches!(ipc.as_str(), "" | "private" | "shareable" | "none") {
            return Err(format!("IpcMode {ipc} is not available to containers here"));
        }
    }
    if let Some(Value::String(c)) = hc.get("CgroupnsMode") {
        if !matches!(c.as_str(), "" | "private") {
            return Err(format!(
                "CgroupnsMode {c} is not available to containers here"
            ));
        }
    }
    if let Some(opts) = hc.get("SecurityOpt").and_then(Value::as_array) {
        for o in opts {
            let o = o.as_str().unwrap_or("");
            if !(o.starts_with("no-new-privileges")) {
                return Err(format!(
                    "security option {o:?} is not available (only no-new-privileges)"
                ));
            }
        }
    }
    if let Some(sys) = hc.get("Sysctls").and_then(Value::as_object) {
        if let Some(k) = sys.keys().find(|k| !k.starts_with("net.")) {
            return Err(format!("sysctl {k} is not available (only net.*)"));
        }
    }
    if let Some(log) = hc.get("LogConfig").and_then(Value::as_object) {
        let ty = log.get("Type").and_then(Value::as_str).unwrap_or("");
        // A remote log driver (syslog, gelf, fluentd …) is a network path out.
        if !matches!(
            ty,
            "" | "json-file" | "k8s-file" | "journald" | "local" | "none"
        ) {
            return Err(format!("log driver {ty} is not available"));
        }
    }

    // Network: only the session's own, or none.
    match hc.get("NetworkMode").and_then(Value::as_str).unwrap_or("") {
        "" | "default" | "bridge" => {
            needs.default_network = true;
        }
        "none" => {}
        m if m == "host" || m.starts_with("container:") || m.starts_with("ns:") => {
            return Err(format!(
                "network mode {m} is not available: containers stay on the session's \
                 internal network"
            ));
        }
        m => needs.networks.push(m.to_string()),
    }
    if let Some(eps) = obj
        .get("NetworkingConfig")
        .and_then(|n| n.get("EndpointsConfig"))
        .and_then(Value::as_object)
    {
        for k in eps.keys() {
            if !matches!(k.as_str(), "bridge" | "default") && !needs.networks.contains(k) {
                needs.networks.push(k.clone());
            }
        }
    }
    let hc = obj
        .get_mut("HostConfig")
        .and_then(Value::as_object_mut)
        .expect("HostConfig was just ensured");

    // Binds.
    if let Some(binds) = hc.get_mut("Binds").and_then(Value::as_array_mut) {
        for b in binds.iter_mut() {
            let s = b
                .as_str()
                .ok_or("a Binds entry is not a string")?
                .to_string();
            let (src, dst, opts) =
                split_bind(&s).ok_or_else(|| format!("bind {s:?} has no target"))?;
            if src.starts_with('/') {
                let host = translate_bind(src, scope, canon)?;
                if let Some(o) = opts {
                    check_bind_opts(o)?;
                }
                let rewritten = match opts {
                    Some(o) => format!("{}:{dst}:{o}", host.display()),
                    None => format!("{}:{dst}", host.display()),
                };
                *b = json!(rewritten);
            } else {
                needs.volumes.push(src.to_string());
            }
        }
    }
    // Mounts.
    if let Some(mounts) = hc.get_mut("Mounts").and_then(Value::as_array_mut) {
        for m in mounts.iter_mut() {
            let m = m.as_object_mut().ok_or("a Mounts entry is not an object")?;
            let ty = m.get("Type").and_then(Value::as_str).unwrap_or("volume");
            match ty {
                "bind" => {
                    let src = m
                        .get("Source")
                        .and_then(Value::as_str)
                        .ok_or("a bind mount has no Source")?;
                    let host = translate_bind(src, scope, canon)?;
                    if let Some(bo) = m.get("BindOptions").and_then(Value::as_object) {
                        let prop = bo.get("Propagation").and_then(Value::as_str).unwrap_or("");
                        if !matches!(prop, "" | "private" | "rprivate") {
                            return refuse(format!("bind propagation {prop} is not available"));
                        }
                        if bo.get("NonRecursive").is_some_and(|v| !is_zero(v)) {
                            // Harmless either way; allowed.
                        }
                    }
                    m.insert("Source".into(), json!(host.display().to_string()));
                }
                "volume" => {
                    if m.get("VolumeOptions")
                        .and_then(|o| o.get("DriverConfig"))
                        .is_some_and(|d| !is_zero(d))
                    {
                        return refuse(
                            "volume driver options are not available (a driver option can \
                             mount a host path)",
                        );
                    }
                    if let Some(src) = m.get("Source").and_then(Value::as_str) {
                        if !src.is_empty() {
                            needs.volumes.push(src.to_string());
                        }
                    }
                }
                "tmpfs" => {}
                other => return refuse(format!("mount type {other} is not available")),
            }
        }
    }
    if let Some(from) = hc.get("VolumesFrom").and_then(Value::as_array) {
        for f in from {
            let c = f.as_str().unwrap_or("");
            needs
                .containers
                .push(c.split(':').next().unwrap_or("").to_string());
        }
    }

    // Published ports: always on the host's loopback, never on every address, and
    // never a fixed host port another session (or your own stack) may hold.
    let publish_all = hc
        .get("PublishAllPorts")
        .is_some_and(|v| v.as_bool() == Some(true));
    let mut bindings = hc
        .get("PortBindings")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if publish_all {
        for p in exposed.iter().chain(image_ports) {
            bindings.entry(p.clone()).or_insert_with(|| json!([{}]));
        }
    }
    for (_, list) in bindings.iter_mut() {
        let Some(list) = list.as_array_mut() else {
            continue;
        };
        if list.is_empty() {
            list.push(json!({}));
        }
        for b in list.iter_mut() {
            if let Some(b) = b.as_object_mut() {
                b.insert("HostIp".into(), json!("127.0.0.1"));
            }
        }
    }
    hc.insert("PublishAllPorts".into(), json!(false));
    if !bindings.is_empty() {
        hc.insert("PortBindings".into(), Value::Object(bindings));
    }
    Ok(needs)
}

fn check_bind_opts(opts: &str) -> Result<(), String> {
    for o in opts.split(',') {
        if matches!(o, "shared" | "rshared" | "slave" | "rslave") {
            return Err(format!("bind propagation {o} is not available"));
        }
    }
    Ok(())
}

/// Check and rewrite `POST /networks/create`: internal (no route out), labelled.
/// Returns the network's name.
pub fn rewrite_network_create(body: &mut Value, scope: &Scope) -> Result<String, String> {
    let obj = body
        .as_object_mut()
        .ok_or("the network definition is not a JSON object")?;
    let name = obj
        .get("Name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .ok_or("a network needs a Name")?
        .to_string();
    let driver = obj.get("Driver").and_then(Value::as_str).unwrap_or("");
    if !matches!(driver, "" | "bridge") {
        return Err(format!("network driver {driver} is not available"));
    }
    if obj.get("Options").is_some_and(|o| !is_zero(o)) {
        return Err("network driver options are not available".into());
    }
    if obj.get("ConfigFrom").is_some_and(|o| !is_zero(o))
        || obj.get("Ingress").is_some_and(|o| !is_zero(o))
    {
        return Err("that kind of network is not available".into());
    }
    obj.insert("Internal".into(), json!(true));
    let labels = obj.entry("Labels").or_insert_with(|| json!({}));
    if labels.is_null() {
        *labels = json!({});
    }
    labels
        .as_object_mut()
        .ok_or("Labels is not an object")?
        .insert(LABEL.into(), json!(scope.session));
    Ok(name)
}

/// Check and rewrite `POST /volumes/create`: local driver, no options, labelled.
/// Returns the volume's name (`None` for an anonymous one).
pub fn rewrite_volume_create(body: &mut Value, scope: &Scope) -> Result<Option<String>, String> {
    let obj = body
        .as_object_mut()
        .ok_or("the volume definition is not a JSON object")?;
    let driver = obj.get("Driver").and_then(Value::as_str).unwrap_or("");
    if !matches!(driver, "" | "local") {
        return Err(format!("volume driver {driver} is not available"));
    }
    if obj.get("DriverOpts").is_some_and(|o| !is_zero(o)) {
        return Err(
            "volume driver options are not available (a driver option can mount a host path)"
                .into(),
        );
    }
    let labels = obj.entry("Labels").or_insert_with(|| json!({}));
    if labels.is_null() {
        *labels = json!({});
    }
    labels
        .as_object_mut()
        .ok_or("Labels is not an object")?
        .insert(LABEL.into(), json!(scope.session));
    Ok(obj
        .get("Name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .map(String::from))
}

/// Check `POST /containers/{id}/exec`.
pub fn check_exec_create(body: &Value) -> Result<(), String> {
    if body.get("Privileged").is_some_and(|v| !is_zero(v)) {
        return Err("privileged exec is not available".into());
    }
    Ok(())
}

/// The registry an image reference names, Docker's way: a first component with a
/// `.` or `:` (or `localhost`) is a registry, otherwise it is Docker Hub.
pub fn registry_of(reference: &str) -> String {
    match reference.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            first.to_ascii_lowercase()
        }
        _ => "docker.io".into(),
    }
}

/// The fully qualified form of a reference, so the engine cannot resolve a short
/// name through its own search list to a registry other than the one checked.
pub fn qualify(reference: &str) -> String {
    if registry_of(reference) != "docker.io" || reference.starts_with("docker.io/") {
        return reference.to_string();
    }
    if reference.contains('/') {
        format!("docker.io/{reference}")
    } else {
        format!("docker.io/library/{reference}")
    }
}

/// Check and rewrite a pull's query (`fromImage`, `tag`).
pub fn rewrite_pull(query: &str, scope: &Scope) -> Result<String, String> {
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    if pairs.iter().any(|(k, _)| k == "fromSrc") {
        return Err("importing an image from a URL is not available".into());
    }
    let image = pairs
        .iter_mut()
        .find(|(k, _)| k == "fromImage")
        .ok_or("a pull needs fromImage")?;
    let registry = registry_of(&image.1);
    if !scope
        .registries
        .iter()
        .any(|r| cowboy_core::policy::domain_matches(r, &registry))
    {
        return Err(format!(
            "pulling from {registry} is not allowed (sandbox.containers.allow_registries)"
        ));
    }
    image.1 = qualify(&image.1);
    Ok(url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish())
}

/// Force a build's `RUN` steps onto no network.
pub fn rewrite_build(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .filter(|(k, _)| k != "networkmode" && k != "remote")
        .collect();
    pairs.push(("networkmode".into(), "none".into()));
    url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish()
}

/// Whether an object's labels say it is this session's.
pub fn owned(labels: Option<&Value>, session: &str) -> bool {
    labels
        .and_then(|l| l.get(LABEL))
        .and_then(Value::as_str)
        .is_some_and(|s| s == session)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            session: "s1".into(),
            paths: vec![
                ("/workspace".into(), "/home/u/proj".into()),
                ("/tmp".into(), "/home/u/.cache/cowboy/scratch/s1/tmp".into()),
            ],
            denied: vec!["/home/u/proj/.cowboy".into()],
            registries: vec!["docker.io".into(), "ghcr.io".into()],
            build: false,
        }
    }

    /// Paths as they are; `/home/u/proj/link` is a symlink to `~/.ssh`.
    fn canon(p: &Path) -> Option<PathBuf> {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix("/home/u/proj/link") {
            return Some(PathBuf::from(format!("/home/u/.ssh{rest}")));
        }
        if s.contains("missing") {
            return None;
        }
        Some(p.to_path_buf())
    }

    fn create(hc: Value) -> Value {
        json!({ "Image": "postgres:16", "HostConfig": hc })
    }

    fn check(mut body: Value) -> Result<(Value, Needs), String> {
        let n = rewrite_container_create(&mut body, &scope(), &[], &canon)?;
        Ok((body, n))
    }

    #[test]
    fn routes_cover_what_compose_and_testcontainers_use() {
        assert_eq!(route("GET", "/v1.41/_ping", false), Route::Pass);
        assert_eq!(route("GET", "/containers/json", false), Route::Scoped);
        assert_eq!(
            route("POST", "/v1.43/containers/create", false),
            Route::CreateContainer
        );
        assert_eq!(
            route("POST", "/containers/abc/start", false),
            Route::ContainerLifecycle("abc".into())
        );
        assert_eq!(
            route("PUT", "/containers/abc/archive", false),
            Route::Container("abc".into())
        );
        assert_eq!(
            route("POST", "/exec/e1/start", false),
            Route::Exec("e1".into())
        );
        assert_eq!(route("POST", "/images/create", false), Route::Pull);
        assert_eq!(
            route("POST", "/networks/n/connect", false),
            Route::NetworkConnect("n".into())
        );
    }

    #[test]
    fn dangerous_or_unknown_routes_are_refused() {
        for (m, p) in [
            ("POST", "/images/x/push"),
            ("DELETE", "/images/postgres"),
            ("POST", "/build"),
            ("POST", "/containers/abc/update"),
            ("GET", "/v4.0.0/libpod/info"),
            ("POST", "/swarm/init"),
            ("POST", "/plugins/pull"),
            ("POST", "/auth"),
            ("POST", "/images/load"),
        ] {
            assert!(matches!(route(m, p, false), Route::Deny(_)), "{m} {p}");
        }
        assert_eq!(route("POST", "/build", true), Route::Build);
    }

    #[test]
    fn a_plain_create_is_labelled_and_put_on_the_session_network() {
        let (body, needs) = check(create(json!({}))).unwrap();
        assert_eq!(body["Labels"][LABEL], "s1");
        assert!(needs.default_network);
        assert_eq!(body["HostConfig"]["PublishAllPorts"], false);
    }

    #[test]
    fn the_session_label_cannot_be_overridden() {
        let mut body = create(json!({}));
        body["Labels"] = json!({ LABEL: "someone-else" });
        let (body, _) = check(body).unwrap();
        assert_eq!(body["Labels"][LABEL], "s1");
    }

    #[test]
    fn every_privilege_field_is_refused_by_name() {
        for (k, v) in [
            ("Privileged", json!(true)),
            ("CapAdd", json!(["SYS_ADMIN"])),
            ("Devices", json!([{"PathOnHost": "/dev/kvm"}])),
            ("PidMode", json!("host")),
            ("UsernsMode", json!("host")),
            ("Runtime", json!("runc")),
            ("MaskedPaths", json!([])),
            ("CgroupParent", json!("/x")),
        ] {
            let e = check(create(json!({ k: v }))).unwrap_err();
            assert!(e.contains(k), "{k}: {e}");
        }
        let e = check(create(json!({ "SecurityOpt": ["seccomp=unconfined"] }))).unwrap_err();
        assert!(e.contains("seccomp"), "{e}");
        let e = check(create(json!({ "Sysctls": {"kernel.shmmax": "1"} }))).unwrap_err();
        assert!(e.contains("kernel.shmmax"), "{e}");
        let e = check(create(json!({ "LogConfig": {"Type": "syslog"} }))).unwrap_err();
        assert!(e.contains("syslog"), "{e}");
        // An unknown field with a value fails closed.
        let e = check(create(json!({ "NewThing": true }))).unwrap_err();
        assert!(e.contains("NewThing"), "{e}");
        // Defaults and harmless options pass.
        check(create(json!({
            "Privileged": false, "CapAdd": null, "SecurityOpt": ["no-new-privileges"],
            "Sysctls": {"net.core.somaxconn": "1024"}, "IpcMode": "private",
        })))
        .unwrap();
    }

    #[test]
    fn host_namespaces_are_refused() {
        for mode in ["host", "container:abc", "ns:/proc/1/ns/net"] {
            let e = check(create(json!({ "NetworkMode": mode }))).unwrap_err();
            assert!(e.contains(mode), "{e}");
        }
        assert!(check(create(json!({ "IpcMode": "host" }))).is_err());
        let (_, n) = check(create(json!({ "NetworkMode": "proj_default" }))).unwrap();
        assert_eq!(n.networks, ["proj_default"]);
        assert!(!n.default_network);
    }

    /// The platform stack's binds: read-only config dirs in the project, and the
    /// project root itself. Translated to host paths the engine can mount.
    #[test]
    fn binds_inside_the_project_are_translated_to_host_paths() {
        let (body, _) = check(create(json!({ "Binds": [
            "/workspace/docker/traefik:/etc/traefik/dynamic:ro",
            "/workspace:/workspace:ro",
            "/tmp/fixtures:/data",
        ]})))
        .unwrap();
        assert_eq!(
            body["HostConfig"]["Binds"],
            json!([
                "/home/u/proj/docker/traefik:/etc/traefik/dynamic:ro",
                "/home/u/proj:/workspace:ro",
                "/home/u/.cache/cowboy/scratch/s1/tmp/fixtures:/data",
            ])
        );
    }

    #[test]
    fn binds_that_escape_are_refused() {
        for (src, why) in [
            ("/", "outside"),
            ("/home/u", "outside"),
            ("/run/user/1000/podman/podman.sock", "outside"),
            ("/workspace/../etc", ".."),
            ("/workspace/link", "resolves"),
            ("/workspace/link/missing/key", "resolves"),
            ("/workspace/.cowboy", ".cowboy"),
            ("/workspace/.cowboy/security.yaml", ".cowboy"),
            ("relative/path", "volume"),
        ] {
            let r = check(create(json!({ "Binds": [format!("{src}:/x")] })));
            if why == "volume" {
                // Not a path: a named volume, checked for ownership instead.
                assert_eq!(r.unwrap().1.volumes, ["relative/path"]);
                continue;
            }
            let e = r.unwrap_err();
            assert!(e.contains(why), "{src}: {e}");
        }
        let e = check(create(json!({ "Binds": ["/workspace/x:/x:rshared"] }))).unwrap_err();
        assert!(e.contains("rshared"), "{e}");
    }

    #[test]
    fn mounts_get_the_same_rules() {
        let e = check(create(
            json!({ "Mounts": [{"Type": "bind", "Source": "/etc"}] }),
        ))
        .unwrap_err();
        assert!(e.contains("outside"), "{e}");
        let e = check(create(json!({ "Mounts": [{"Type": "volume", "Source": "v",
            "VolumeOptions": {"DriverConfig": {"Name": "local", "Options": {"device": "/"}}}}] })))
        .unwrap_err();
        assert!(e.contains("driver"), "{e}");
        let e = check(create(
            json!({ "Mounts": [{"Type": "npipe", "Source": "x"}] }),
        ))
        .unwrap_err();
        assert!(e.contains("npipe"), "{e}");
        let (body, n) = check(create(json!({ "Mounts": [
            {"Type": "bind", "Source": "/workspace/sql", "Target": "/sql"},
            {"Type": "volume", "Source": "pgdata", "Target": "/var/lib/postgresql"},
            {"Type": "tmpfs", "Target": "/run"},
        ]})))
        .unwrap();
        assert_eq!(
            body["HostConfig"]["Mounts"][0]["Source"],
            "/home/u/proj/sql"
        );
        assert_eq!(n.volumes, ["pgdata"]);
    }

    /// Testcontainers asks for `PublishAllPorts`, which binds every host address.
    /// The proxy binds the exposed ports to `127.0.0.1` instead, and pins every
    /// explicit binding there too.
    #[test]
    fn published_ports_bind_the_host_loopback_only() {
        let mut body = json!({
            "Image": "postgres:16",
            "ExposedPorts": {"5432/tcp": {}},
            "HostConfig": {"PublishAllPorts": true, "PortBindings": {
                "8080/tcp": [{"HostIp": "0.0.0.0", "HostPort": "8080"}]
            }}
        });
        rewrite_container_create(&mut body, &scope(), &["9000/tcp".into()], &canon).unwrap();
        let pb = &body["HostConfig"]["PortBindings"];
        assert_eq!(pb["5432/tcp"][0]["HostIp"], "127.0.0.1");
        assert_eq!(pb["9000/tcp"][0]["HostIp"], "127.0.0.1");
        assert_eq!(pb["8080/tcp"][0]["HostIp"], "127.0.0.1");
        assert_eq!(body["HostConfig"]["PublishAllPorts"], false);
    }

    #[test]
    fn networks_are_internal_and_labelled() {
        let mut body = json!({"Name": "proj_default", "Driver": "bridge", "Internal": false});
        assert_eq!(
            rewrite_network_create(&mut body, &scope()).unwrap(),
            "proj_default"
        );
        assert_eq!(body["Internal"], true);
        assert_eq!(body["Labels"][LABEL], "s1");
        for bad in [
            json!({"Name": "n", "Driver": "macvlan"}),
            json!({"Name": "n", "Options": {"com.docker.network.bridge.name": "eth0"}}),
            json!({"Name": "n", "ConfigFrom": {"Network": "x"}}),
        ] {
            assert!(
                rewrite_network_create(&mut bad.clone(), &scope()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn volumes_take_no_driver_options() {
        let mut body = json!({"Name": "pgdata"});
        assert_eq!(
            rewrite_volume_create(&mut body, &scope())
                .unwrap()
                .as_deref(),
            Some("pgdata")
        );
        assert_eq!(body["Labels"][LABEL], "s1");
        let mut bad =
            json!({"Name": "v", "DriverOpts": {"type": "none", "o": "bind", "device": "/"}});
        assert!(rewrite_volume_create(&mut bad, &scope()).is_err());
        let mut bad = json!({"Name": "v", "Driver": "nfs"});
        assert!(rewrite_volume_create(&mut bad, &scope()).is_err());
    }

    #[test]
    fn pulls_are_limited_to_allowed_registries_and_qualified() {
        assert_eq!(registry_of("postgres"), "docker.io");
        assert_eq!(registry_of("bitnami/redis"), "docker.io");
        assert_eq!(registry_of("ghcr.io/x/y"), "ghcr.io");
        assert_eq!(registry_of("localhost:5000/x"), "localhost:5000");
        let q = rewrite_pull("fromImage=postgres&tag=16", &scope()).unwrap();
        assert!(
            q.contains("fromImage=docker.io%2Flibrary%2Fpostgres"),
            "{q}"
        );
        assert!(rewrite_pull("fromImage=ghcr.io%2Fa%2Fb&tag=1", &scope()).is_ok());
        let e = rewrite_pull("fromImage=evil.example%2Fx&tag=1", &scope()).unwrap_err();
        assert!(e.contains("evil.example"), "{e}");
        assert!(rewrite_pull("fromSrc=http://x/y.tar", &scope()).is_err());
    }

    #[test]
    fn listings_are_scoped_to_the_session() {
        let q = scope_query("all=1", "s1");
        let filters: Value = url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "filters")
            .map(|(_, v)| serde_json::from_str(&v).unwrap())
            .unwrap();
        assert_eq!(filters["label"], json!(["cowboy.session=s1"]));
        // The caller's own filters are kept, and narrowed.
        let q = scope_query(
            &format!(
                "filters={}",
                url::form_urlencoded::byte_serialize(
                    br#"{"label":["com.docker.compose.project=p"]}"#
                )
                .collect::<String>()
            ),
            "s1",
        );
        assert!(q.contains("com.docker.compose.project"), "{q}");
        assert!(q.contains("cowboy.session%3Ds1"), "{q}");
    }

    #[test]
    fn exec_cannot_be_privileged() {
        assert!(check_exec_create(&json!({"Cmd": ["ls"]})).is_ok());
        assert!(check_exec_create(&json!({"Privileged": true})).is_err());
    }

    #[test]
    fn builds_get_no_network() {
        let q = rewrite_build("t=x&networkmode=host&remote=http://x");
        assert!(q.contains("networkmode=none") && !q.contains("host") && !q.contains("remote"));
    }

    #[test]
    fn ownership_reads_the_label() {
        assert!(owned(Some(&json!({LABEL: "s1"})), "s1"));
        assert!(!owned(Some(&json!({LABEL: "s2"})), "s1"));
        assert!(!owned(Some(&json!({})), "s1"));
        assert!(!owned(None, "s1"));
    }
}
