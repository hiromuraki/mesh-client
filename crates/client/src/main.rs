//! mesh-client：进程形态。
//!
//! 对应 Go 的 main.go。两种形态的唯一差别是有没有托盘：Windows 默认桌面模式，
//! 托盘常驻；--headless（以及所有非 Windows 平台）不起托盘，面板照样监听
//! 127.0.0.1，地址落在 config/ui.json，用 ssh -L 或 curl 访问。

#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod boot_check;
mod cli;
mod follow_log;
mod headless;
mod instance;
mod platform;
#[cfg(windows)]
mod tray;

use mesh_engine::{Cmd, Config, Notice, Published, Shutdown, StateCell};
use mesh_install::layout;
use mesh_panel::Panel;
use mesh_state::Store;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

pub(crate) const APP_TITLE: &str = "Mesh Client";

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let options = match cli::parse(&argv) {
        Ok(options) => options,
        Err(cli::ParseError::Help) => {
            platform::say(&cli::usage());
            return ExitCode::SUCCESS;
        }
        Err(cli::ParseError::Invalid(message)) => {
            platform::say_err(&format!("{APP_TITLE}: {message}"));
            return ExitCode::FAILURE;
        }
    };
    // 非 Windows 上没有托盘，也不该在没有桌面的机器上试着打开浏览器：
    // 一律走 headless（面板仍可经 ssh 转发访问）。
    let desktop = !options.headless && cfg!(windows);
    if !desktop {
        platform::attach_console();
    }
    match run(options, desktop) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            platform::say_err(&format!("{APP_TITLE}: {message}"));
            if desktop {
                platform::notify_error(&message);
            }
            ExitCode::FAILURE
        }
    }
}

fn run(options: cli::Options, desktop: bool) -> Result<(), String> {
    let root = resolve_root(options.root.as_deref())?;
    // 安装不完整就直接失败：读不到 bin/version.json 就不知道要报哪一版给服务端。
    let version = layout::read_version(&root).map_err(|err| {
        mesh_text::format(
            mesh_text::id::CLIENT_ERROR_INSTALL_DIR,
            &[&root.display(), &err],
        )
    })?;
    if options.version {
        platform::say(&format!("{APP_TITLE} {version}"));
        return Ok(());
    }
    let store = Store::open(&root).map_err(|err| err.to_string())?;
    let _lock = match mesh_install::lock::acquire(&mesh_install::lock::Lock::path(&root)) {
        Ok(lock) => lock,
        Err(mesh_install::Error::AlreadyRunning) => {
            return instance::wake_existing(
                &root.join(layout::CONFIG).join("ui.json"),
                platform::open_browser,
            );
        }
        Err(err) => return Err(err.to_string()),
    };

    let shutdown = Shutdown::new();
    let cell = Arc::new(StateCell::new(Published::default()));
    let (notice_tx, notice_rx) = channel::<Notice>();
    let config = Config {
        root: root.clone(),
        self_version: version.clone(),
        hostname: platform::hostname(),
        auto_update_disabled: options.disable_auto_update,
    };
    let (tx, _engine) =
        mesh_engine::spawn(config, Arc::clone(&cell), notice_tx, Arc::clone(&shutdown));
    if desktop {
        platform::spawn_notice_thread(notice_rx);
    } else {
        drop(notice_rx);
    }

    let outcome = serve(
        &options, desktop, &root, &version, &store, &cell, &tx, &shutdown,
    );
    // 两种结局都要收尾。Unix 上 core 在自己的会话里，进程直接退会留下一个占着
    // 虚拟 IP 的孤儿——headless 收到 SIGTERM 后 serve 是正常返回的，只在失败路径
    // 收尾会让每次 systemctl stop 都漏掉 core。
    // run() 幂等：/api/quit 与更新交接已经走过时，这里只是一次空转。
    shutdown.run();
    outcome
}

#[allow(clippy::too_many_arguments)]
fn serve(
    options: &cli::Options,
    desktop: bool,
    root: &Path,
    version: &str,
    store: &Store,
    cell: &Arc<StateCell>,
    tx: &Sender<Cmd>,
    shutdown: &Arc<Shutdown>,
) -> Result<(), String> {
    configure(tx, store, options, desktop)?;

    // quit 是 /api/quit 的动作：必须先回 204，面板在 send 之后才调用它。
    let quit_shutdown = Arc::clone(shutdown);
    let quit: mesh_panel::Quit = Box::new(move || quit_shutdown.run_and_exit(0));
    let log_path = root.join(layout::LOG).join("client.log");
    let panel = mesh_panel::start(Arc::clone(cell), tx.clone(), log_path.clone(), quit)
        .map_err(|err| mesh_text::format(mesh_text::id::CLIENT_ERROR_START_PANEL, &[&err]))?;
    let panel_closer = panel.clone();
    shutdown.register(move || panel_closer.close());
    let _ = mesh_panel::write_url(&root.join(layout::CONFIG).join("ui.json"), &panel.url);

    // 安装器在等这个标记来判断新版本「能不能正常跑起来」：一看到它就删掉 bin.old。
    // 所以时机不能只看「引擎与面板已就位」——进程形态本身（托盘）也得立起来，否则
    // 更新已提交、客户端随即退出，下一次登录同样失败，而回滚材料已经没了。
    let ready = boot_ready(root, options.boot_check.as_deref());

    if desktop {
        return desktop_main(&panel, root, version, cell, tx, shutdown, ready);
    }
    platform::say(&mesh_text::format(
        mesh_text::id::CLIENT_TEXT_PANEL_URL,
        &[&APP_TITLE, &panel.url],
    ));
    headless::run(Arc::clone(cell), &log_path, ready)
}

/// boot_ready 返回「这一版算成功启动」的回调。它只生效一次，可以重复调用。
fn boot_ready(root: &Path, token: Option<&str>) -> Box<dyn Fn() + Send + Sync> {
    let Some(token) = token.filter(|token| !token.is_empty()) else {
        return Box::new(|| {});
    };
    let root = root.to_path_buf();
    let token = token.to_string();
    let done = AtomicBool::new(false);
    Box::new(move || {
        if done.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(message) = boot_check::write_marker(&root, &token) {
            platform::say_err(&format!("{APP_TITLE}: {message}"));
        }
    })
}

#[cfg(windows)]
fn desktop_main(
    panel: &Panel,
    root: &Path,
    version: &str,
    cell: &Arc<StateCell>,
    tx: &Sender<Cmd>,
    shutdown: &Arc<Shutdown>,
    ready: Box<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let quit_shutdown = Arc::clone(shutdown);
    tray::run(tray::TrayOptions {
        url: panel.url.clone(),
        root: root.to_path_buf(),
        version: version.to_string(),
        cell: Arc::clone(cell),
        tx: tx.clone(),
        quit: Arc::new(move || quit_shutdown.run_and_exit(0)),
        ready,
    })
}

#[cfg(not(windows))]
fn desktop_main(
    _panel: &Panel,
    _root: &Path,
    _version: &str,
    _cell: &Arc<StateCell>,
    _tx: &Sender<Cmd>,
    _shutdown: &Arc<Shutdown>,
    _ready: Box<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    Err(mesh_text::get(mesh_text::id::CLIENT_TEXT_NO_DESKTOP).to_string())
}

/// 把参数合并到已有配置上再提交给引擎；没给的字段沿用本地值。
fn configure(
    tx: &Sender<Cmd>,
    store: &Store,
    options: &cli::Options,
    desktop: bool,
) -> Result<(), String> {
    let current = store.load_client().map_err(|err| err.to_string())?;
    let given = !options.server.trim().is_empty()
        || !options.token.trim().is_empty()
        || !options.name.trim().is_empty()
        || !options.core.trim().is_empty();
    if !given {
        // 还没来得及注册不算错误：桌面模式下面板会停在设置页让用户填服务端
        // 地址与令牌。headless 没人能填，只能直接失败。
        let unregistered = current
            .as_ref()
            .map(|config| config.device_id.is_empty())
            .unwrap_or(true);
        if unregistered && !desktop {
            return Err(mesh_text::get(mesh_text::id::CLIENT_ERROR_NOT_REGISTERED).to_string());
        }
        return Ok(());
    }
    let mut input = mesh_engine::SettingsInput {
        server_url: options.server.trim().to_string(),
        token: options.token.trim().to_string(),
        hostname: options.name.trim().to_string(),
        core_path: options.core.trim().to_string(),
    };
    if let Some(current) = current.as_ref() {
        if input.server_url.is_empty() {
            input.server_url = current.server_url.clone();
        }
        if input.hostname.is_empty() {
            input.hostname = current.hostname.clone();
        }
        if input.core_path.is_empty() {
            input.core_path = current.core_path.clone();
        }
    }
    let (reply_tx, reply_rx) = channel();
    tx.send(Cmd::Settings(input, reply_tx))
        .map_err(|_| mesh_text::get(mesh_text::id::CLIENT_ERROR_ENGINE_NOT_RUNNING).to_string())?;
    match reply_rx.recv() {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(message)) => Err(message),
        Err(_) => Err(mesh_text::get(mesh_text::id::CLIENT_ERROR_ENGINE_NOT_RUNNING).to_string()),
    }
}

/// 确定安装根目录：启动器会显式给出；直接运行 bin/ 里的主程序时，根就是 bin 的上一级。
fn resolve_root(given: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = given {
        return if path.is_absolute() {
            Ok(path.to_path_buf())
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .map_err(|err| mesh_text::format(mesh_text::id::CLIENT_ERROR_CURRENT_DIR, &[&err]))
        };
    }
    let exe = std::env::current_exe()
        .map_err(|err| mesh_text::format(mesh_text::id::CLIENT_ERROR_EXE_PATH, &[&err]))?;
    Ok(layout::find_root(&exe))
}
