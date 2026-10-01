//! Web UI 服务模式（`--server`）：不创建任何窗口，零 WebView2 依赖。
//!
//! std 手写 HTTP（每连接一线程，全部 Connection: close）：
//! - `POST /api/invoke/<cmd>` —— 等价 Tauri `invoke`，`{ok,data}` / `{ok,error}` 包裹
//! - `GET  /api/events`（SSE）—— 等价 Tauri `listen`，帧为 `{"event":...,"payload":...}`
//! - `POST /api/emit/<event>` —— 浏览器侧 `emit`（如验证码弹窗回传）
//! - 其余 GET —— 静态托管 vite 产物（`ZSW_WEB_DIST` 或 exe/cwd 相邻的 `dist/`）
//!
//! 只绑 127.0.0.1；启动生成一次性 token（URL 查询参数或 `x-zsw-token` 头）鉴权。
//! 复用 store 层：与桌面模式走同一套 `store_guard()` 互斥 + 原子写，多标签页并发天然串行化。

use crate::cipher;
use crate::claim;
use crate::flowlog;
use crate::i18n;
use crate::oauth::{self, PollOutcome};
use crate::store::{self, Paths};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const MAX_BODY: u64 = 64 * 1024 * 1024;
const PICK_ZCODE_PATH_UNSUPPORTED: &str = "web 模式不支持文件选择对话框：请在输入框直接填写 ZCode 路径";
const AUTOSTART_UNSUPPORTED: &str = "web 模式不支持开机自启（由桌面模式管理）";

struct SseClient(Mutex<TcpStream>);
static SSE_CLIENTS: Mutex<Vec<Arc<SseClient>>> = Mutex::new(Vec::new());

/// 启动 token（一次性，进程内常量）。放在静态里便于所有响应统一种 cookie。
static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn token_matches(c: &str) -> bool {
    match TOKEN.get() {
        Some(t) => !c.is_empty() && c == t,
        None => false,
    }
}

fn cookie_token(header: Option<&str>) -> Option<String> {
    header?.split(';').find_map(|p| {
        p.trim()
            .strip_prefix("zsw-token=")
            .map(|v| percent_decode(v).trim().to_string())
    })
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 向所有 SSE 客户端广播事件（等价桌面模式的 `app.emit`）。
pub fn emit_event(name: &str, payload: &Value) {
    let frame = format!("data: {}\n\n", json!({ "event": name, "payload": payload }));
    let mut clients = lock(&SSE_CLIENTS);
    clients.retain(|c| {
        let mut w = lock(&c.0);
        w.write_all(frame.as_bytes()).and_then(|_| w.flush()).is_ok()
    });
}

/// Web UI 访问地址（含 token），供托盘"打开界面"复用
static WEB_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn web_url() -> Option<&'static str> {
    WEB_URL.get().map(|s| s.as_str())
}

/// 启动 HTTP 服务（绑端口、起 accept 线程、写地址文件、自动开浏览器）。
/// 不阻塞：由 Tauri setup 在服务模式下调用，进程生命周期由托盘/事件循环接管。
pub fn spawn() {
    let paths = Paths::detect();
    i18n::init_from_settings(&store::load_settings(&paths));
    flowlog::init(&paths.store_dir());

    let token = uuid::Uuid::new_v4().simple().to_string();
    let _ = TOKEN.set(token.clone());
    let port: u16 = std::env::var("ZSW_SERVER_PORT").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[zsw-server] 绑定 127.0.0.1 失败：{e}");
            std::process::exit(1);
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let dist = resolve_dist_dir();
    let url = format!("http://127.0.0.1:{port}/?token={token}");
    let _ = WEB_URL.set(url.clone());
    println!("[zsw-server] UI 地址  {url}");
    println!("[zsw-server] 静态目录  {}", dist.display());
    // release exe 无控制台（windows_subsystem），把带 token 的地址落盘兜底
    let _ = std::fs::create_dir_all(paths.store_dir());
    let _ = std::fs::write(paths.store_dir().join("web-ui-url.txt"), &url);
    if std::env::var("ZSW_NO_OPEN").ok().as_deref() != Some("1") {
        let _ = store::open_url(&url);
    }
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let dist = dist.clone();
            std::thread::spawn(move || handle_conn(stream, &dist));
        }
    });
}

/// 非 Tauri 的独立运行形态（`run_server()`，仅调试用）：spawn 后永久阻塞。
pub fn run_server() {
    spawn();
    loop {
        std::thread::park();
    }
}

fn resolve_dist_dir() -> PathBuf {
    if let Ok(d) = std::env::var("ZSW_WEB_DIST") {
        if !d.trim().is_empty() {
            return PathBuf::from(d);
        }
    }
    let mut cands: Vec<PathBuf> = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        cands.push(cwd.join("dist"));
        cands.push(cwd.join("..").join("dist"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent() {
            cands.push(p.join("dist"));
            cands.push(p.join("..").join("dist"));
        }
    }
    for c in cands {
        if c.join("index.html").exists() {
            return c.canonicalize().unwrap_or(c);
        }
    }
    PathBuf::from("dist")
}

/// 语言只按首个请求的 Accept-Language 探测一次；设置里显式选过的语言优先。
static LANG_PROBED: AtomicBool = AtomicBool::new(false);

fn maybe_detect_lang(header: Option<&str>) {
    if header.is_none() || LANG_PROBED.swap(true, Ordering::Relaxed) {
        return;
    }
    if store::load_settings(&Paths::detect()).language.is_some() {
        return;
    }
    let first = header
        .unwrap_or("")
        .split(',')
        .next()
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let lang = i18n::Lang::parse(&first)
        .unwrap_or(if first.starts_with("zh") { i18n::Lang::Zh } else { i18n::Lang::En });
    i18n::set(lang);
}

struct Req {
    method: String,
    target: String,
    token_header: Option<String>,
    cookie_header: Option<String>,
    accept_language: Option<String>,
    body: Vec<u8>,
}

fn handle_conn(stream: TcpStream, dist: &Path) {
    if !stream.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false) {
        return;
    }
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let Some(req) = parse_request(&mut reader) else {
        return;
    };
    route(stream, req, dist);
}

fn parse_request(reader: &mut BufReader<TcpStream>) -> Option<Req> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_ascii_uppercase();
    let target = parts.next()?.to_string();
    if method.is_empty() || target.is_empty() {
        return None;
    }
    let mut content_length: usize = 0;
    let mut token_header: Option<String> = None;
    let mut cookie_header: Option<String> = None;
    let mut accept_language: Option<String> = None;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let t = h.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some((k, v)) = t.split_once(':') {
            let (k, v) = (k.trim(), v.trim());
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("x-zsw-token") {
                token_header = Some(v.trim_start_matches("Bearer ").to_string());
            } else if k.eq_ignore_ascii_case("cookie") {
                cookie_header = Some(v.to_string());
            } else if k.eq_ignore_ascii_case("accept-language") {
                accept_language = Some(v.to_string());
            }
        }
    }
    if content_length as u64 > MAX_BODY {
        return None;
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Req { method, target, token_header, cookie_header, accept_language, body })
}

fn route(mut stream: TcpStream, req: Req, dist: &Path) {
    maybe_detect_lang(req.accept_language.as_deref());
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.target.clone(), String::new()),
    };
    let qp = parse_query(&query);
    // 鉴权三通道：x-zsw-token 头 → URL ?token= → zsw-token cookie（首次静态响应种下）
    let authed = [
        req.token_header.as_deref(),
        qp.iter().find(|(k, _)| k == "token").map(|(_, v)| v.as_str()),
        cookie_token(req.cookie_header.as_deref()).as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(token_matches);

    if path == "/api/events" && req.method == "GET" {
        if !authed {
            return respond(&mut stream, 401, "text/plain; charset=utf-8", b"unauthorized");
        }
        return handle_sse(stream);
    }
    if let Some(cmd) = path.strip_prefix("/api/invoke/") {
        if req.method != "POST" {
            return respond(&mut stream, 405, "text/plain; charset=utf-8", b"method not allowed");
        }
        if !authed {
            return respond(&mut stream, 401, "text/plain; charset=utf-8", b"unauthorized");
        }
        let args: Value = serde_json::from_slice(&req.body).unwrap_or_else(|_| json!({}));
        let (status, payload) = match dispatch(cmd, args) {
            Ok(v) => (200, json!({ "ok": true, "data": v })),
            Err(e) => (400, json!({ "ok": false, "error": e })),
        };
        return respond_json(&mut stream, status, &payload);
    }
    if let Some(name) = path.strip_prefix("/api/emit/") {
        if !authed {
            return respond(&mut stream, 401, "text/plain; charset=utf-8", b"unauthorized");
        }
        let payload: Value =
            serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        emit_event(&percent_decode(name), &payload);
        return respond_json(&mut stream, 200, &json!({ "ok": true }));
    }
    if path == "/oauth/callback" {
        // web 模式下 zcode:// deeplink 不可达，OAuth 完成依赖服务端轮询通道
        let page = "<!doctype html><meta charset=\"utf-8\"><body style=\"background:#0a0a0c;color:#ddd;font-family:sans-serif;padding:2em\">\
        此回调仅桌面模式使用。Web 模式下登录结果会经轮询通道自动写回，若授权页跳转到这里报错，可直接关闭本页并回到主界面等待。</body>";
        return respond(&mut stream, 200, "text/html; charset=utf-8", page.as_bytes());
    }
    if req.method == "GET" {
        return serve_static(&mut stream, dist, &path);
    }
    respond(&mut stream, 404, "text/plain; charset=utf-8", b"not found");
}

fn handle_sse(stream: TcpStream) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\nretry: 2000\n\n";
    {
        let mut s = &stream;
        if s.write_all(head.as_bytes()).and_then(|_| s.flush()).is_err() {
            return;
        }
    }
    let client = Arc::new(SseClient(Mutex::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    })));
    lock(&SSE_CLIENTS).push(client.clone());
    let mut reader = BufReader::new(stream);
    let mut buf = [0u8; 512];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    lock(&SSE_CLIENTS).retain(|c| !Arc::ptr_eq(c, &client));
}

fn serve_static(stream: &mut TcpStream, dist: &Path, path: &str) {
    let rel = path.trim_start_matches('/');
    let rel: String = if rel.is_empty() { "index.html".to_string() } else { percent_decode(rel) };
    if rel.split(['/', '\\']).any(|seg| seg == "..") {
        return respond(stream, 400, "text/plain; charset=utf-8", b"bad path");
    }
    let full = dist.join(&rel);
    let full = if full.is_dir() { full.join("index.html") } else { full };
    match std::fs::read(&full) {
        Ok(bytes) => respond(stream, 200, mime_of(&full), &bytes),
        Err(_) => respond(stream, 404, "text/plain; charset=utf-8", b"not found"),
    }
}

fn mime_of(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8]) {
    // 给所有响应种 token cookie：之后不带 ?token= 的手动打开也能过鉴权（仅 127.0.0.1 可达）
    let cookie = TOKEN.get()
        .map(|t| format!("Set-Cookie: zsw-token={t}; Path=/; SameSite=Strict\r\n"))
        .unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n{}Connection: close\r\n\r\n",
        status,
        status_text(status),
        ctype,
        body.len(),
        cookie
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn respond_json(stream: &mut TcpStream, status: u16, payload: &Value) {
    let body = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    respond(stream, status, "application/json; charset=utf-8", body.as_bytes());
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| -> Option<u8> {
                    match b {
                        b'0'..=b'9' => Some(b - b'0'),
                        b'a'..=b'f' => Some(b - b'a' + 10),
                        b'A'..=b'F' => Some(b - b'A' + 10),
                        _ => None,
                    }
                };
                if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn arg<'a>(args: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|n| args.get(n))
}

fn arg_str(args: &Value, names: &[&str]) -> Option<String> {
    arg(args, names).and_then(|v| v.as_str()).map(String::from)
}

fn arg_bool(args: &Value, names: &[&str]) -> Option<bool> {
    arg(args, names).and_then(|v| v.as_bool())
}

fn to_value<T: serde::Serialize>(v: Result<T, String>) -> Result<Value, String> {
    v.and_then(|x| serde_json::to_value(x).map_err(|e| e.to_string()))
}

/// 导出落盘：相对路径 → 返回内容交浏览器下载；绝对路径 → 服务端原子直写。
fn export_result(path: &str, body: &str, extra: Value) -> Result<Value, String> {
    if Path::new(path).is_absolute() {
        store::atomic_write(Path::new(path), body)
            .map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
        let mut v = json!({ "saved": true, "path": path });
        if let (Some(obj), Some(extra)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in extra {
                obj.insert(k.clone(), val.clone());
            }
        }
        Ok(v)
    } else {
        Ok(json!({
            "saved": false,
            "download": true,
            "filename": path,
            "content": body,
        }))
    }
}

/// invoke 分发：语义与 lib.rs 的 `#[tauri::command]` 一一对应
///（store_guard 互斥、改后 emit state-changed），仅把 AppHandle 换成 SSE 广播。
fn dispatch(cmd: &str, args: Value) -> Result<Value, String> {
    let paths = Paths::detect();
    match cmd {
        "get_state" => to_value(store::get_state(&paths)),
        "app_version" => Ok(json!(env!("CARGO_PKG_VERSION"))),

        "capture_current" => {
            let _g = crate::store_guard();
            let r = to_value(store::capture_current(&paths, arg_str(&args, &["name"])));
            emit_event("state-changed", &Value::Null);
            r
        }
        "rename_account" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let name = arg_str(&args, &["name"]).unwrap_or_default();
            let _g = crate::store_guard();
            let r = to_value(store::rename_account(&paths, &id, &name));
            emit_event("state-changed", &Value::Null);
            r
        }
        "delete_account" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let _g = crate::store_guard();
            let r = to_value(store::delete_account(&paths, &id));
            emit_event("state-changed", &Value::Null);
            r
        }
        "update_account_from_live" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let _g = crate::store_guard();
            let r = to_value(store::update_account_from_live(&paths, &id));
            emit_event("state-changed", &Value::Null);
            r
        }

        // 核心切换：纯本地文件 + 进程操作，与桌面模式共用同一段实现
        "switch_to" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let force = arg_bool(&args, &["force"]).unwrap_or(false);
            let restart = arg_bool(&args, &["restart"]).unwrap_or(false);
            let _g = crate::store_guard();
            let hot = store::load_settings(&paths).hot_switch();
            to_value(store::switch_to(&paths, &id, force, restart, hot))
        }

        "get_live_quota" => to_value(store::live_quota(&paths)),
        "get_account_quota" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            to_value(store::account_quota(&paths, &id))
        }

        "claim_preview" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let mid = store::ensure_virtual_device_mid(&paths, &id)?;
            let acc = store::load_account(&paths, &id)?;
            to_value(claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid)))
        }
        "claim_refresh" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let mid = store::ensure_virtual_device_mid(&paths, &id)?;
            let acc = store::load_account(&paths, &id)?;
            let (activated, activation_error) =
                match claim::telemetry_user_id(&paths.home, &acc.credentials) {
                    Some(uid) => match claim::report_activation_events(&uid, &mid) {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e)),
                    },
                    None => (false, None),
                };
            let plans = claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid))?;
            Ok(json!({ "plans": plans, "activated": activated, "activationError": activation_error }))
        }
        // 里程碑⑤：验证码弹窗由浏览器 window.open 承载（captcha.html 静态托管），
        // pending 状态与桌面模式共用同一把 PENDING_CLAIM；结果经 SSE 广播 claim://result
        "claim_start" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let plan_id = arg_str(&args, &["planId", "plan_id"]).ok_or("missing planId")?;
            let mid = store::ensure_virtual_device_mid(&paths, &id)?;
            let acc = store::load_account(&paths, &id)?;
            let plans =
                claim::preview_plans(&paths.home, &acc.credentials, acc.config.as_ref(), Some(mid.clone()))?;
            let plan = plans
                .iter()
                .find(|p| p.plan_id == plan_id)
                .ok_or_else(|| i18n::tr("err.claim.gone"))?;
            let display = if plan.name.is_empty() { plan.plan_id.clone() } else { plan.name.clone() };
            *crate::pending_guard() = Some(crate::PendingClaim {
                account_id: acc.id.clone(),
                account_name: acc.name.clone(),
                plan_id: plan.plan_id.clone(),
                plan_name: display.clone(),
                credentials: acc.credentials,
                config: acc.config,
                device_mid: mid,
            });
            Ok(json!({ "account": acc.name, "plan": display }))
        }
        "claim_captcha_submit" => {
            let param = arg_str(&args, &["param"]).ok_or("missing param")?;
            let region = arg_str(&args, &["region"]);
            let pending = crate::pending_guard()
                .take()
                .ok_or_else(|| i18n::tr("err.claim.none_pending"))?;
            let res = claim::submit_claim(
                &paths.home,
                &pending.credentials,
                pending.config.as_ref(),
                &pending.plan_id,
                &param,
                region.as_deref(),
                Some(pending.device_mid.clone()),
            );
            let payload = crate::claim_result_payload(&pending, res);
            emit_event("claim://result", &payload);
            Ok(payload)
        }
        "claim_cancel" => {
            *crate::pending_guard() = None;
            Ok(Value::Null)
        }
        "claim_captcha_config" => to_value(claim::fetch_captcha_config()),

        "oauth_providers" => Ok(json!(oauth::OAUTH_PROVIDERS)),
        "oauth_begin" => {
            let provider = arg_str(&args, &["provider"]).ok_or("missing provider")?;
            server_oauth_begin(&provider)
        }

        "set_auth_proxy" => {
            let on = arg_bool(&args, &["on"]).unwrap_or(false);
            let url = arg_str(&args, &["url"]);
            let _g = crate::store_guard();
            let trimmed = url.as_deref().map(str::trim).filter(|s| !s.is_empty());
            let normalized = match trimmed {
                Some(s) => Some(oauth::parse_proxy_url(s)?),
                None => None,
            };
            if on && normalized.is_none() {
                return Err(i18n::tr("err.proxy.need_url"));
            }
            let mut s = store::load_settings(&paths);
            s.auth_proxy_on = Some(on);
            s.auth_proxy_url = normalized;
            store::save_settings(&paths, &s)?;
            drop(_g);
            emit_event("state-changed", &Value::Null);
            Ok(Value::Null)
        }

        "kill_zcode" => {
            let _g = crate::store_guard();
            let r = if store::kill_zcode()? { Ok(Value::Null) } else { Err(i18n::tr("err.zcode.kill_timeout")) };
            r
        }
        "set_behavior" => {
            let _g = crate::store_guard();
            let mut s = store::load_settings(&paths);
            if let Some(v) = arg_bool(&args, &["launchAfterSwitch", "launch_after_switch"]) {
                s.launch_after_switch = Some(v);
            }
            if let Some(v) = arg_bool(&args, &["closeToTray", "close_to_tray"]) {
                s.close_to_tray = Some(v);
            }
            if let Some(v) = arg_bool(&args, &["hotSwitch", "hot_switch"]) {
                s.hot_switch = Some(v);
            }
            if let Some(v) = arg_bool(&args, &["autoClaim", "auto_claim"]) {
                s.auto_claim = Some(v);
            }
            let r = store::save_settings(&paths, &s);
            drop(_g);
            emit_event("state-changed", &Value::Null);
            to_value(r)
        }
        "set_language" => {
            let lang = arg_str(&args, &["lang"]).unwrap_or_default();
            let l = i18n::Lang::parse(&lang)
                .ok_or_else(|| i18n::trf("err.lang.unknown", &[("lang", &lang)]))?;
            {
                let _g = crate::store_guard();
                let mut s = store::load_settings(&paths);
                s.language = Some(l.as_str().to_string());
                store::save_settings(&paths, &s)?;
            }
            i18n::set(l);
            emit_event("state-changed", &Value::Null);
            Ok(Value::Null)
        }

        "autostart_status" => Ok(json!(false)),
        "autostart_set" => Err(AUTOSTART_UNSUPPORTED.into()),

        // 里程碑③：无文件对话框。导出的 pick 返回建议文件名，finalize 对相对路径
        // 返回加密内容交浏览器 <a download> 保存；绝对路径仍走服务端 atomic_write
        "export_pick_path" => {
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let acc = store::load_account(&paths, &id)?;
            Ok(json!({
                "picked": true,
                "path": format!("{}.zsb", crate::sanitize_filename(&acc.name)),
                "name": acc.name,
            }))
        }
        "export_all_pick_path" => {
            let accounts = store::list_accounts(&paths)?;
            if accounts.is_empty() {
                return Err(i18n::tr("err.export.empty"));
            }
            Ok(json!({ "picked": true, "path": "zcode-accounts.zsb", "count": accounts.len() }))
        }
        "export_finalize" => {
            let path = arg_str(&args, &["path"]).ok_or("missing path")?;
            let id = arg_str(&args, &["id"]).ok_or("missing id")?;
            let password = arg_str(&args, &["password"]).unwrap_or_default();
            let acc = store::load_account(&paths, &id)?;
            let payload = store::export_bundle_value(std::slice::from_ref(&acc));
            let sealed = cipher::seal(&payload, &password, cipher::FORMAT_BUNDLE)?;
            let body = serde_json::to_string_pretty(&sealed).unwrap() + "\n";
            export_result(&path, &body, json!({}))
        }
        "export_all_finalize" => {
            let path = arg_str(&args, &["path"]).ok_or("missing path")?;
            let password = arg_str(&args, &["password"]).unwrap_or_default();
            let accounts = store::list_accounts(&paths)?;
            if accounts.is_empty() {
                return Err(i18n::tr("err.export.empty_short"));
            }
            let payload = store::export_bundle_value(&accounts);
            let sealed = cipher::seal(&payload, &password, cipher::FORMAT_BUNDLE)?;
            let body = serde_json::to_string_pretty(&sealed).unwrap() + "\n";
            export_result(&path, &body, json!({ "count": accounts.len() }))
        }
        "import_pick_files" => Err("web 模式导入直接使用浏览器的文件选择器，无需此命令".into()),
        "pick_zcode_path" => Err(PICK_ZCODE_PATH_UNSUPPORTED.into()),
        "import_sealed" => {
            let files = arg(&args, &["files"])
                .and_then(|v| v.as_array())
                .map(|a| -> Vec<(String, Value)> {
                    a.iter()
                        .filter_map(|item| {
                            let arr = item.as_array()?;
                            Some((arr.first()?.as_str()?.to_string(), arr.get(1)?.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let password = arg_str(&args, &["password"]).unwrap_or_default();
            let _g = crate::store_guard();
            let mut decrypted = vec![];
            let mut errors: Vec<String> = vec![];
            for (fname, v) in files {
                match cipher::open(&v, &password) {
                    Ok(payload) => decrypted.push((fname, payload)),
                    Err(e) => errors.push(i18n::trf("err.import.wrap", &[("fname", fname.as_str()), ("e", &e)])),
                }
            }
            let mut report = if decrypted.is_empty() {
                store::ImportReport::default()
            } else {
                store::import_values(&paths, &decrypted)?
            };
            report.picked = true;
            report.errors.extend(errors);
            to_value(Ok(report))
        }

        "set_zcode_path" => {
            let path = arg_str(&args, &["path"]).unwrap_or_default();
            let _g = crate::store_guard();
            let mut s = store::load_settings(&paths);
            s.zcode_path = store::normalize_zcode_path(&path);
            let r = store::save_settings(&paths, &s);
            drop(_g);
            emit_event("state-changed", &Value::Null);
            to_value(r)
        }
        "launch_zcode" => {
            let (p, ok) = store::effective_zcode_path(&paths);
            if !ok {
                return Err(i18n::trf("err.zcode.path_invalid_hint", &[("p", &p)]));
            }
            to_value(store::launch_zcode(&p))
        }
        "open_external" => {
            let url = arg_str(&args, &["url"]).ok_or("missing url")?;
            to_value(store::open_url(&url))
        }
        "reveal_main" | "open_settings" => Ok(Value::Null),

        _ => Err(format!("unknown command: {cmd}")),
    }
}

fn server_oauth_begin(provider: &str) -> Result<Value, String> {
    if !oauth::OAUTH_PROVIDERS.iter().any(|p| p.id == provider) {
        return Err(i18n::trf("err.oauth.unknown_provider", &[("provider", provider)]));
    }
    *crate::pending_oauth_guard() = None;
    let flow = uuid::Uuid::new_v4().to_string();
    flowlog::log(&flow, "begin", &format!("provider={provider} mode=web proxy=browser"));
    let mid = uuid::Uuid::new_v4().to_string();
    let init = oauth::init_flow(provider, &mid)?;
    flowlog::log(&flow, "init-ok", "web");
    *crate::pending_oauth_guard() = Some(crate::PendingOAuth {
        provider: provider.to_string(),
        state: init.state.clone(),
        flow: flow.clone(),
    });
    let provider_owned = provider.to_string();
    std::thread::spawn(move || {
        server_poll_loop(provider_owned, flow, mid, init.poll_url.clone(), init.poll_token.clone(), init.expires_at_ms, init.poll_interval_ms);
    });
    Ok(json!({ "opened": true, "provider": provider, "url": init.authorize_url }))
}

/// 与 lib.rs::spawn_poll_loop 同构，但完成后经 SSE 广播 oauth://done（无窗口可关）。
fn server_poll_loop(
    provider: String,
    flow: String,
    mid: String,
    url: String,
    poll_token: String,
    expires_at_ms: u128,
    interval_ms: u64,
) {
    let ours = || {
        crate::pending_oauth_guard()
            .as_ref()
            .map(|p| p.flow == flow)
            .unwrap_or(false)
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let deadline = std::cmp::min(expires_at_ms, now + u128::from(oauth::FLOW_TIMEOUT_MS));
    loop {
        if !ours() {
            flowlog::log(&flow, "poll-exit", "flow-done-or-replaced");
            return;
        }
        match oauth::poll_flow_once(&url, &poll_token, &mid) {
            Ok(PollOutcome::Pending) => {}
            Ok(PollOutcome::Ready(data)) => {
                flowlog::log(&flow, "poll-ready", "web");
                let raw = json!({ "code": 0, "data": data });
                let result = crate::persist_oauth_account(&Paths::detect(), &provider, &raw, &flow, &mid, true);
                *crate::pending_oauth_guard() = None;
                match result {
                    Ok(v) => emit_event("oauth://done", &v),
                    Err(e) if e != "__superseded__" => emit_event("oauth://done", &json!({ "ok": false, "error": e })),
                    Err(_) => {}
                }
                return;
            }
            Err(e) => {
                if !ours() {
                    flowlog::log(&flow, "poll-exit", "superseded");
                    return;
                }
                flowlog::log(&flow, "poll-fail", &e);
                *crate::pending_oauth_guard() = None;
                emit_event("oauth://done", &json!({ "ok": false, "error": e }));
                return;
            }
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        if now >= deadline {
            if !ours() {
                return;
            }
            flowlog::log(&flow, "poll-timeout", "");
            *crate::pending_oauth_guard() = None;
            emit_event("oauth://done", &json!({ "ok": false, "error": i18n::tr("err.oauth.expired") }));
            return;
        }
        let sleep = (deadline - now).min(interval_ms as u128) as u64;
        std::thread::sleep(std::time::Duration::from_millis(sleep));
    }
}
