#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
fn ensure_console(attach: bool) {
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetConsoleCP, SetConsoleOutputCP, SetStdHandle,
        ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };
    const CONOUT: &[u16] = &[67, 79, 78, 79, 85, 84, 36, 0];
    const NUL: &[u16] = &[78, 85, 76, 0];
    unsafe {
        let cur = GetStdHandle(STD_OUTPUT_HANDLE);
        if cur != 0 && cur != INVALID_HANDLE_VALUE {
            // 已有控制台（终端里直接运行）：仅切 UTF-8
            SetConsoleOutputCP(65001);
            SetConsoleCP(65001);
            return;
        }
        // attach=true（--cli）：挂到父进程控制台输出日志
        // attach=false（--server）：脱离父控制台，防止关 cmd 窗口连带杀掉服务；
        //                          stdout 落到 NUL，println! 不 panic
        let ok = attach && AttachConsole(ATTACH_PARENT_PROCESS) != 0;
        let target: &[u16] = if ok { CONOUT } else { NUL };
        let h = CreateFileW(
            target.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            0,
        );
        if h != INVALID_HANDLE_VALUE {
            SetStdHandle(STD_OUTPUT_HANDLE, h);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
        // Rust 输出是 UTF-8，把控制台切到 UTF-8 避免 println! 乱码
        SetConsoleOutputCP(65001);
        SetConsoleCP(65001);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--cli") {
        let cli_args: Vec<String> = args.iter().skip(pos + 1).cloned().collect();
        #[cfg(windows)]
        ensure_console(true);
        let (out, code) = zcode_switch_lib::cli::run(&cli_args);
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
        std::process::exit(code);
    }
    // release 默认 = Web UI 服务模式（无窗口 + 托盘常驻 + 自动开浏览器，不依赖 WebView2），
    // --desktop 才起桌面 WebView 窗口；--server 仍被接受，兼容旧启动脚本。
    // debug 构建（npm run tauri dev）保持原默认 = 桌面窗口，避免开发时变成无窗口服务模式。
    let server_mode = if cfg!(debug_assertions) {
        args.iter().any(|a| a == "--server")
    } else {
        !args.iter().any(|a| a == "--desktop")
    };
    if server_mode {
        ensure_console_is_detached();
    }
    zcode_switch_lib::run(server_mode)
}

// Web UI 服务模式：脱离父控制台，双击/开机自启时不留黑窗；关 cmd 窗口不影响服务
#[cfg(windows)]
fn ensure_console_is_detached() {
    ensure_console(false);
}

#[cfg(not(windows))]
fn ensure_console_is_detached() {}
