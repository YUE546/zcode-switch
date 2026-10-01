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

## Web UI 化改造（no-webview2 分支，进行中）

目标：前端改跑在任意浏览器里，由 Rust 后端提供 HTTP 服务，产品不再创建任何 Tauri WebView 窗口（Windows 上即不再要求 WebView2 运行时）。

**已落地（本分支，CI 已跑通 2026-10-01）**：
- `src/bridge.js` 双模式传输桥：`invoke/listen/emit` 三个签名不变；Tauri 模式动态加载 @tauri-apps/api 直通，浏览器模式走 HTTP + SSE。业务文件只改了 import。token 兜底顺序：URL `?token=` → `zsw-token` cookie（服务端所有响应都会种）→ localStorage。
- `src-tauri/src/server.rs` 服务模式：std 手写 HTTP（每连接一线程、Connection: close），`POST /api/invoke/<cmd>` = invoke（`{ok,data}`/`{ok,error}`），`GET /api/events` SSE = listen，`POST /api/emit/<e>` = emit，其余 GET 静态托管 `dist/`（`ZSW_WEB_DIST` 或 exe/cwd 旁）。绑 127.0.0.1 + 一次性 token（也落盘 `~/.zcode-switch/web-ui-url.txt`）。dispatch 与 lib.rs command 语义一一对应（store_guard 互斥、emit state-changed），OAuth 复用 `persist_oauth_account` + 服务端轮询通道。
- 服务模式形态：`zcode-switch.exe --server` 跑**无窗口的 Tauri 应用 + 托盘常驻**（`lib.rs::run(server_mode)` 里 `context.config_mut().app.windows.clear()`——零 WebView 窗口，仍不要求 WebView2 运行时；`.run(context)` 必须传清空后的 context）。托盘"打开界面" = `open_url(WEB_URL)`；关 cmd 窗口不影响服务（`main.rs::ensure_console(false)` 不 AttachConsole，stdout 落 NUL）。桌面/服务共用单实例锁，二者同时只能跑一个。
- 启动方式（2026-10-03 起）：**release 双击 exe 默认即服务模式**（main.rs 无参 → `run(true)`），`--desktop` 才开桌面 WebView 窗口，`--server` 保留兼容旧脚本；**debug 构建（`npm run tauri dev`）默认仍是桌面窗口**，避免开发时变成无窗口服务。注意：桌面模式注册的开机自启是裸 exe 路径，装了本版后开机自启会进服务模式并自动开浏览器。
- `open_url` 放行 `http://127.0.0.1`/`http://localhost`（原 https-only 会静默拒绝服务模式自动开浏览器 → 曾致"unauthorized"）；控制台 UTF-8（`SetConsoleOutputCP(65001)`）。
- web 模式暂缺：开机自启（桌面专属，settings 页在浏览器模式下隐藏该开关；`autostart_set` 仍报错兜底）。OAuth 回调 zcode:// 在浏览器不可达属预期，轮询通道兜底。
- **③⑤ 已实现（2026-10-01，前端 vite build + check:i18n 本地过；Rust 侧待 Gitee CI 编译验证）**：导出 = `export_pick_path`/`export_all_pick_path` 在服务端 dispatch 返回**建议文件名**（非绝对路径），`export_finalize`/`export_all_finalize` 对相对路径返回 `{download:true, filename, content}` 交浏览器 `<a download>`（ui.js `downloadText`），绝对路径仍服务端 atomic_write；导入 = settings.js `importFilesWeb()` 用 `<input type=file>` 读文件、前端按 cipher 信封（kdf+cipher / format=zsw-accounts-bundle）预检后走 `import_sealed`；验证码 = server.rs dispatch 实现 `claim_start`（存 `PENDING_CLAIM`，与桌面共用 `pending_guard()`）/`claim_captcha_submit`（复用 lib.rs `claim_result_payload` + SSE emit `claim://result`）/`claim_cancel`；前端 bridge.js `openCaptchaWindow()`（**必须在点击手势内同步调用**占位 about:blank 防弹窗拦截）→ `claim_start` 成功后 `loadCaptchaWindow()` 载入 captcha.html → 提交后弹窗自关（桌面由后端关窗）；弹窗被拦截时 `claim_cancel` + toast `m.captchaPopupBlocked`。
- CI：`.workflow/build-webui-server.yml`（Gitee Go）在 no-webview2 分支 push 时交叉编译 windows-gnu release 并打包 exe+dist+说明（`release-web/README-WEB.md`；启动脚本 start-webui.bat 已于 2026-10-03 随"默认即服务模式"移除）。要点：`--features custom-protocol`（不经 tauri build 必须开，否则桌面模式内嵌资源失效）；mingw 装完**硬校验 dlltool**（曾因 apt 静默失败 + `| head` 管道吞退出码，编译期才炸"error calling dlltool"）；补 windows-rs 大写库别名；从 webview2-com-sys crate 源码目录复制 `x64/WebView2Loader.dll` 随包（windows-gnu 动态链接它）；`APT::Keep-Downloaded-Packages "true"` 让 .deb 留在平台缓存（22.04+ 默认装完即删，否则每轮重下 119MB mingw）。产物双层封装（tgz 内含 `.tar_<构建号>`）是平台行为。

**原始方案与里程碑**（①bridge ②全量 command ③文件对话框 ④OAuth ⑤验证码 ⑥关窗口）：

有利前提（已核实）：前端仅依赖 `@tauri-apps/api` 的 `invoke`/`listen`/`emit` 三个函数，无其他 Tauri DOM API；页面本身是纯静态 Vite 产物。

1. **传输层桥接**：`src/bridge.js`，与 `@tauri-apps/api` 同签名。浏览器实现 = `POST /api/invoke/<cmd>`（JSON，`Result<T,String>` 映射 200/400）+ SSE `GET /api/events`（透传现有 5 个事件名与 payload）。三个入口只改 import，业务代码零改动。✅
2. **后端加 HTTP 服务**：同步风格（std::sync::Mutex / std::thread / ureq），std 手写 tiny HTTP 最贴合（不引入 tokio）；同时托管 `dist/`。**只绑 127.0.0.1 + 启动 token 鉴权**（此 API 能切号/杀进程）。⚠️ 每请求一线程是硬要求——`switch_to` 可阻塞 90 秒，逐请求串行会卡死所有标签页。✅
3. **文件对话框改浏览器语义**：export → fetch blob + `<a download>`；import → `<input type=file>` 上传；`pick_zcode_path` 文本框 + 服务端校验。之后可移除 `tauri-plugin-dialog`。✅（桌面模式保留对话框插件；web 分支见上）
4. **OAuth 改浏览器登录**：`oauth_begin` 返回授权 URL，前端 `window.open`；完成依赖服务端轮询通道（`spawn_poll_loop` 同构），`zcode://` deeplink 浏览器不可达属预期；如 provider 风控拦浏览器，退化"手动粘贴回调 URL"。✅
5. **验证码页**：`captcha.html` 服务端托管，`window.open` 弹小窗，`captcha://interactive` 链路不变。✅（见上；自动领取无点击手势，弹窗被拦则按需人工验证处理）
6. **窗口收敛**：把 lib.rs 所有 `WebviewWindowBuilder`（main/settings/captcha/login）收进 feature 或 `--server` 分支关掉——这是"不依赖 WebView2"的最终开关；托盘/自启/`--cli` 保留在桌面模式。✅（`--server` 经 `config_mut().app.windows.clear()` 零窗口；托盘两种模式共用）
7. **收尾**：CSP 去 ipc 项、语言探测 Accept-Language、vite proxy `/api`（已加，`ZSW_SERVER_PORT=xxx npm run dev`）。✅（2026-10-01：csp/devCsp 的 `ipc: http://ipc.localhost` 已删——Tauri v2 会自动注入，项目只禁了 style-src 的 CSP 改写；server.rs `maybe_detect_lang()` 按首个请求的 Accept-Language 探测一次，设置里显式选过的语言优先）
