# AGENTS.md — zcode-switch (Z·SWITCH)

Tauri 2 桌面工具：在多个 ZCode 账号间一键切换并展示额度。改的是 ZCode 的 `~/.zcode/v2/credentials.json`（只换登录身份，项目/会话/设置不动）。Windows 优先，但 CI 要求 Linux/macOS 也能编译。无框架、无打包器之外的构建层：原生 ES Module + Vite + 手写 CSS。

## 目录

- `index.html` / `settings.html` / `captcha.html` — 三个独立 Vite 入口（见 `vite.config.js` 的 `rollupOptions.input`），对应主窗 / 设置窗 / 验证码窗
- `src/` — 前端（无框架）：
  - `main.js` 账号列表/额度/领取/自动领取主逻辑；`render()` 整体 innerHTML 重绘
  - `settings.js` 设置页；`captcha.js` 阿里云验证码页（动态加载 alicdn SDK）
  - `ui.js` 共享 UI：`esc()` 转义、`toast()`、模态框、**事件白名单分发**（`click`/`keydown`/`blur` 属性 → `window.*` 函数，不用内联 JS 以过 CSP）
  - `i18n.js` + `locales/{zh,en}.js` 双语字典；`icons.js` 内联 SVG
- `src-tauri/src/` — Rust 后端：
  - `lib.rs` 全部 Tauri command（34 个）+ 托盘 + 窗口创建 + 事件 emit
  - `store.rs` 账号库读写、切换逻辑、原子写（tmp + rename）、路径白名单
  - `oauth.rs` BigModel/z.ai OAuth（登录窗 `WebviewUrl::External` + `zcode://` deeplink 回调 + 轮询）
  - `quota.rs` / `claim.rs` 额度查询与活动领取（含验证码窗口联动）；`probe.rs` ZCode 路径/进程探测；`cli.rs` `--cli` 子命令；`cipher.rs`/`zcrypto.rs` `.zsb` 加密导出；`i18n.rs` Rust 侧文案（与前端 key 对应）；`flowlog.rs` OAuth 流程日志

## 命令

```bash
npm install
npm run tauri dev        # 开发（先起 vite 127.0.0.1:5173 strictPort，再挂 Tauri 窗口）
npm run tauri build      # NSIS 安装包（报 os error 32 = 旧实例驻留托盘锁 exe，退出重试）
npm run dist             # 校验三处版本一致 → tauri build → 拷贝 setup/portable 到 release/
npm run check:i18n       # 校验 zh/en key 对齐 + 所有 t("...") 调用 key 存在（改文案后必跑）
cargo test --lib         # 在 src-tauri/ 下；CI 三平台跑，目前无 #[test]，实为编译检查
```

无 lint / typecheck / 前端测试配置；包管理用 npm（README 与 dist.mjs 均按 npm 写）。

## 架构边界与约定

- **前后端唯一通道**：前端只经 `invoke()`（`@tauri-apps/api/core`）调 lib.rs 里的 command，只经 `listen()` 收 5 个事件：`tray-action`、`claim://result`、`captcha://interactive`、`oauth://done`、`state-changed`。新功能优先加 command + 事件，别绕过这层。
- **加新 command 的模式**：`store_guard()`（全局 `STORE_LOCK` 互斥）包住所有读改写 → 改完 `rebuild_tray(&app)` + `app.emit("state-changed", ())`。落盘一律走 `store::atomic_write`。
- **安全红线**：账号 id 只允许 `[A-Za-z0-9-]`（防路径穿越）；凭据解密仅用于显示；导出走 PBKDF2+AES-GCM；切换前必须先保全 live 登录（防丢号）。改 store.rs 时别破坏这些。
- **i18n**：任何用户可见文案需同时进 `locales/zh.js`、`locales/en.js`；Rust 侧文案在 `i18n.rs`（zh/en 两张表）。改完跑 `npm run check:i18n`。
- **CSP**：`tauri.conf.json` 的 `csp`/`devCsp` 是白名单制，`script-src` 基线 `'self'` + 阿里云验证码域名；新引第三方脚本/图片要最小化放行。前端事件分发走 `ui.js` 的 `runAttr`（只支持 `window.xxx` 函数 + 字面量参数或 `event`），handler 挂到 `window` 上（如 `window.actions`）。
- **版本号三处同步**：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`，`npm run dist` 会强校验。
- **平台兼容**：路径探测/进程管理是 Win32 语义；非 Windows 分支用 `#[cfg(not(windows))]`（如时区用 `iana-time-zone`）。Windows 子进程要 `creation_flags(0x0800_0000)` 防弹黑窗（见 store.rs `no_window`）。
- **路径环境变量**：`ZCODE_SWITCH_HOME`（账号库根）、`ZCODE_SWITCH_DATA_ROOT` / `ZCODE_DATA_BASE_DIR`（live 数据根，还会读 ZCode `setting.json` 的 `dataBaseDir`）。测试时可用它们隔离。

## Web UI 化改造计划（去掉 WebView2 依赖）

目标：前端改跑在任意浏览器里，由 Rust 后端提供 HTTP 服务，产品不再创建任何 Tauri WebView 窗口（Windows 上即不再要求 WebView2 运行时）。

有利前提（已核实）：前端仅依赖 `@tauri-apps/api` 的 `invoke`/`listen`/`emit` 三个函数，无其他 Tauri DOM API；页面本身是纯静态 Vite 产物。

1. **传输层桥接**：新建 `src/bridge.js`，导出与 `@tauri-apps/api` 同签名的 `invoke(cmd, args)` 和 `listen(event, cb)`。浏览器实现 = `POST /api/invoke/<cmd>`（JSON，`Result<T,String>` 映射 200/400）+ SSE `GET /api/events`（透传现有 5 个事件名与 payload）。三个入口只改 import，业务代码零改动。
2. **后端加 HTTP 服务**：新增 `zsw-server` 形态（同一 exe 加 `--server` 或拆 thin bin）。现有后端是同步风格（std::sync::Mutex / std::thread / ureq），配 `tiny_http` + 手写 SSE 最贴合现状，不必引入 tokio/axum；同时用该服务静态托管 `dist/`，浏览器开 `http://127.0.0.1:<port>` 即得完整 UI。**只绑 127.0.0.1 + 随机端口 + 启动 token 鉴权**（此 API 能切号/杀进程）。
3. **文件对话框改浏览器语义**：`export_pick_path`/`export_all_pick_path` → 前端 `fetch` 结果 blob + `<a download>`；`import_pick_files` → `<input type=file>` 上传字节给服务端；`pick_zcode_path` 保留为文本输入框 + 服务端校验。之后可移除 `tauri-plugin-dialog`。
4. **OAuth 改浏览器登录**：`oauth_begin` 返回授权 URL，前端 `window.open` 新标签页；回调从 `zcode://` deeplink 改为 `http://127.0.0.1:<port>/oauth/callback`（需在 oauth.rs 里适配 redirect_uri 与 provider 注册约束；轮询循环 `spawn_poll_loop` 已存在，结果仍走 `oauth://done` 事件）。风险：浏览器标签没有原登录窗的隔离 profile 与固定 UA（`oauth::LOGIN_WINDOW_UA`），若 provider 风控拦截，退化为"用户手动粘贴回调 URL"兜底。
5. **验证码页**：`captcha.html` 本就是静态页，改由服务端托管，`window.open` 弹小窗；`captcha.js` 的 invoke/emit 换 bridge，`captcha://interactive` 事件链路不变。
6. **窗口收敛**：`settings.html` 变路由/新标签；`reveal_main`/`open_settings` 变 no-op 或路由跳转。把 lib.rs 里所有 `WebviewWindowBuilder`（main/settings/captcha/login）收进 feature 或 `--server` 分支关掉——这是"不依赖 WebView2"的开关点；托盘/自启/单实例/`--cli` 全保留在宿主里，不受影响。
7. **收尾**：CSP 去掉 `ipc:` 相关项改为纯 web 版；语言探测加 `Accept-Language` 兜底；`npm run dev` 增加 vite proxy `/api` 便于纯浏览器开发调试。

里程碑顺序：①bridge + get_state 打通 → ②全部 34 个 command 走 HTTP → ③文件导入导出 → ④OAuth 浏览器化 → ⑤验证码弹窗 → ⑥feature 关闭全部窗口发布 server 模式。
