// 双模式传输桥：业务代码只认 invoke / listen / emit 三个签名。
// - Tauri 桌面模式：直通 @tauri-apps/api（动态加载，浏览器模式下不会打包执行）
// - 浏览器模式：走本机 zsw-server（--server 启动）的 HTTP + SSE
//   invoke  → POST /api/invoke/<cmd>          结果 {ok:true,data} / {ok:false,error}
//   listen  → GET  /api/events（SSE）          帧 {"event":...,"payload":...}
//   emit    → POST /api/emit/<event>           （如验证码弹窗回传主界面）

import { t } from "./i18n.js";

const isTauri = typeof window !== "undefined" && !!window.__TAURI_INTERNALS__;

let tapi = null;
function tauriApi() {
  if (!tapi) {
    tapi = Promise.all([
      import("@tauri-apps/api/core"),
      import("@tauri-apps/api/event"),
    ]).then(([core, event]) => ({
      invoke: core.invoke,
      listen: event.listen,
      emit: event.emit,
    }));
  }
  return tapi;
}

function cookieToken() {
  const m = document.cookie.match(/(?:^|;\s*)zsw-token=([^;]+)/);
  return m ? decodeURIComponent(m[1]) : "";
}

function token() {
  if (typeof window === "undefined") return "";
  const fromUrl = new URLSearchParams(window.location.search).get("token");
  if (fromUrl) {
    try { localStorage.setItem("zsw-token", fromUrl); } catch {}
    return fromUrl;
  }
  // 服务端会给所有响应种 zsw-token cookie，作为 ?token= 缺失时的兜底
  const fromCookie = cookieToken();
  if (fromCookie) {
    try { localStorage.setItem("zsw-token", fromCookie); } catch {}
    return fromCookie;
  }
  try { return localStorage.getItem("zsw-token") || ""; } catch { return ""; }
}

function withToken(path) {
  const t = token();
  return t ? `${path}${path.includes("?") ? "&" : "?"}token=${encodeURIComponent(t)}` : path;
}

async function httpInvoke(cmd, args) {
  const res = await fetch(`/api/invoke/${encodeURIComponent(cmd)}`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-zsw-token": token() },
    body: JSON.stringify(args || {}),
  });
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch {}
  if (res.status === 401) {
    throw t("err.webTokenInvalid");
  }
  if (!res.ok || (data && data.ok === false)) {
    throw (data && data.error) || text || `HTTP ${res.status}`;
  }
  return data ? data.data : null;
}

export async function invoke(cmd, args) {
  if (isTauri) {
    const { invoke: tinvoke } = await tauriApi();
    return tinvoke(cmd, args);
  }
  if (cmd === "open_settings") {
    window.open(withToken("/settings.html"), "_blank");
    return null;
  }
  return httpInvoke(cmd, args);
}

let es = null;
const handlers = new Map();
function sse() {
  if (!es) {
    es = new EventSource(withToken("/api/events"));
    es.onmessage = (ev) => {
      let m = null;
      try { m = JSON.parse(ev.data); } catch { return; }
      const list = handlers.get(m.event);
      if (list) for (const h of [...list]) h({ payload: m.payload });
    };
    // 断线由 EventSource 自动重连，重连后仍用同一批前端 handler
    es.onerror = () => {};
  }
  return es;
}

export async function listen(event, cb) {
  if (isTauri) {
    const { listen: tlisten } = await tauriApi();
    return tlisten(event, cb);
  }
  sse();
  const list = handlers.get(event) || [];
  list.push(cb);
  handlers.set(event, list);
  return () => handlers.set(event, list.filter((h) => h !== cb));
}

export async function emit(event, payload) {
  if (isTauri) {
    const { emit: temit } = await tauriApi();
    return temit(event, payload);
  }
  const res = await fetch(`/api/emit/${encodeURIComponent(event)}`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-zsw-token": token() },
    body: JSON.stringify(payload ?? null),
  });
  if (!res.ok) throw `HTTP ${res.status}`;
}

// web 模式验证码小窗（桌面模式由后端开隐藏窗口，这里返回 null）。
// 先以 about:blank 占住窗口名——必须在点击手势内同步调用防弹窗拦截；
// pending claim 就绪（claim_start 成功）后再 loadCaptchaWindow 载入验证码页。
export function openCaptchaWindow() {
  if (isTauri) return null;
  return window.open("about:blank", "zsw-captcha", "width=430,height=400");
}

export function loadCaptchaWindow(win) {
  if (!win || win.closed) return;
  win.location.href = withToken("/captcha.html");
}

export { isTauri };
