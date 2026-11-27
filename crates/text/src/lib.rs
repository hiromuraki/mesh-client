//! 文案地板：语言清单、按 id 查表与占位符替换。
//!
//! 规范见 `docs/i18n.md`。三条约定：
//!
//! - 文案只存在于 `lang/*.lang`，代码里只出现 [`id`] 的常量；
//! - [`get`] / [`format`] 的 id 参数是 `&'static str`，运行时拼出来的 key 传不进来；
//! - 当前只装载中文。英文清单已随 crate 嵌入并参与测试，但不参与运行。

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// 中文清单：当前运行时唯一装载的表。
const CHINESE_LANG: &str = include_str!("../lang/chinese.lang");
/// 英文清单：与中文同 key，已就位。当前运行时不读，嵌在这里是为了让文件缺失或
/// 解析失败在编译期与测试里立刻暴露。
#[allow(dead_code)]
const ENGLISH_LANG: &str = include_str!("../lang/english.lang");

/// 首次调用时装载一次的 `key → 文案` 表。
static CHINESE: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();

/// 清单里的全部 id。命名是 key 全大写、`.` 换成 `_`。
pub mod id {
    // core
    pub const CORE_ERROR_ASSIGN_JOB: &str = "core.error.assign_job";
    pub const CORE_ERROR_ASSIGN_JOB_LIFECYCLE: &str = "core.error.assign_job_lifecycle";
    pub const CORE_ERROR_CORE_NOT_FOUND: &str = "core.error.core_not_found";
    pub const CORE_ERROR_CREATE_JOB: &str = "core.error.create_job";
    pub const CORE_ERROR_IDENTITY_MISMATCH: &str = "core.error.identity_mismatch";
    pub const CORE_ERROR_LAUNCH: &str = "core.error.launch";
    pub const CORE_ERROR_LINE_TOO_LONG: &str = "core.error.line_too_long";
    pub const CORE_ERROR_OPEN_PROCESS: &str = "core.error.open_process";
    pub const CORE_ERROR_OUTPUT_INTERRUPTED: &str = "core.error.output_interrupted";
    pub const CORE_ERROR_PROCESS_EXITED: &str = "core.error.process_exited";
    pub const CORE_ERROR_PROCESS_STUCK: &str = "core.error.process_stuck";
    pub const CORE_ERROR_READY_TIMEOUT: &str = "core.error.ready_timeout";
    pub const CORE_ERROR_SET_JOB: &str = "core.error.set_job";
    pub const CORE_ERROR_SIGNAL: &str = "core.error.signal";
    pub const CORE_ERROR_STOP_FAILED: &str = "core.error.stop_failed";
    pub const CORE_ERROR_TASKKILL_CALL: &str = "core.error.taskkill_call";
    pub const CORE_ERROR_TASKKILL_STATUS: &str = "core.error.taskkill_status";
    pub const CORE_ERROR_WAIT_EXIT: &str = "core.error.wait_exit";
    pub const CORE_ERROR_WAIT_FAILED: &str = "core.error.wait_failed";
    pub const CORE_TEXT_ABNORMAL_EXIT: &str = "core.text.abnormal_exit";
    pub const CORE_TEXT_EXIT_CODE: &str = "core.text.exit_code";
    pub const CORE_TEXT_KILLED_BY_SIGNAL: &str = "core.text.killed_by_signal";
    pub const CORE_TEXT_NODE_ONLINE: &str = "core.text.node_online";
    pub const CORE_TEXT_PROCESS_EXITED: &str = "core.text.process_exited";
    pub const CORE_TEXT_TIMEOUT: &str = "core.text.timeout";

    // etrpc
    pub const ETRPC_ERROR_CONNECT: &str = "etrpc.error.connect";
    pub const ETRPC_ERROR_EXECUTION_FAILED: &str = "etrpc.error.execution_failed";
    pub const ETRPC_ERROR_FRAME: &str = "etrpc.error.frame";
    pub const ETRPC_ERROR_FRAME_LENGTH: &str = "etrpc.error.frame_length";
    pub const ETRPC_ERROR_METHOD_NOT_FOUND: &str = "etrpc.error.method_not_found";
    pub const ETRPC_ERROR_MISSING_PIECE: &str = "etrpc.error.missing_piece";
    pub const ETRPC_ERROR_PACKET_MALFORMED: &str = "etrpc.error.packet_malformed";
    pub const ETRPC_ERROR_PAYLOAD_TOO_LARGE: &str = "etrpc.error.payload_too_large";
    pub const ETRPC_ERROR_PIECE_COUNT: &str = "etrpc.error.piece_count";
    pub const ETRPC_ERROR_PROTOCOL: &str = "etrpc.error.protocol";
    pub const ETRPC_ERROR_REQUEST_DECODE: &str = "etrpc.error.request_decode";
    pub const ETRPC_ERROR_RESPONSE_ENCODE: &str = "etrpc.error.response_encode";
    pub const ETRPC_ERROR_RESPONSE_TOO_LARGE: &str = "etrpc.error.response_too_large";
    pub const ETRPC_ERROR_RPC_FAILED: &str = "etrpc.error.rpc_failed";
    pub const ETRPC_ERROR_RPC_NO_REASON: &str = "etrpc.error.rpc_no_reason";
    pub const ETRPC_ERROR_RPC_TIMEOUT: &str = "etrpc.error.rpc_timeout";
    pub const ETRPC_ERROR_RPC_UNKNOWN_TYPE: &str = "etrpc.error.rpc_unknown_type";
    pub const ETRPC_ERROR_RPC_UNPARSABLE: &str = "etrpc.error.rpc_unparsable";
    pub const ETRPC_ERROR_SERVICE_NOT_FOUND: &str = "etrpc.error.service_not_found";
    pub const ETRPC_ERROR_TIMEOUT: &str = "etrpc.error.timeout";
    pub const ETRPC_ERROR_UNEXPECTED_PACKET: &str = "etrpc.error.unexpected_packet";

    // install
    pub const INSTALL_ERROR_ALREADY_RUNNING: &str = "install.error.already_running";
    pub const INSTALL_ERROR_MANIFEST_READ: &str = "install.error.manifest_read";
    pub const INSTALL_ERROR_NO_PARENT_DIR: &str = "install.error.no_parent_dir";
    pub const INSTALL_ERROR_TEMP_NAME_COLLISION: &str = "install.error.temp_name_collision";
    pub const INSTALL_ERROR_UNSUPPORTED_PLATFORM: &str = "install.error.unsupported_platform";
    pub const INSTALL_ERROR_VERSION_INVALID: &str = "install.error.version_invalid";

    // remote
    pub const REMOTE_ERROR_API: &str = "remote.error.api";
    pub const REMOTE_ERROR_BODY_TOO_LARGE: &str = "remote.error.body_too_large";
    pub const REMOTE_ERROR_CONFIG_DECODE: &str = "remote.error.config_decode";
    pub const REMOTE_ERROR_CONFIG_NOT_UTF8: &str = "remote.error.config_not_utf8";
    pub const REMOTE_ERROR_CREATE_FILE: &str = "remote.error.create_file";
    pub const REMOTE_ERROR_DECODE: &str = "remote.error.decode";
    pub const REMOTE_ERROR_READ_RESPONSE: &str = "remote.error.read_response";
    pub const REMOTE_ERROR_TRANSPORT: &str = "remote.error.transport";
    pub const REMOTE_ERROR_WRITE_FILE: &str = "remote.error.write_file";

    // state
    pub const STATE_ERROR_CREDENTIAL_CIPHER_NOT_BASE64: &str =
        "state.error.credential_cipher_not_base64";
    pub const STATE_ERROR_CREDENTIAL_DPAPI_UNREADABLE: &str =
        "state.error.credential_dpapi_unreadable";
    pub const STATE_ERROR_CREDENTIAL_NOT_BASE64: &str = "state.error.credential_not_base64";
    pub const STATE_ERROR_CREDENTIAL_PLAINTEXT: &str = "state.error.credential_plaintext";
    pub const STATE_ERROR_EMPTY_CREDENTIAL_SECRET: &str = "state.error.empty_credential_secret";
    pub const STATE_ERROR_EMPTY_ROOT: &str = "state.error.empty_root";
    pub const STATE_ERROR_IO: &str = "state.error.io";
    pub const STATE_ERROR_KEY_LENGTH: &str = "state.error.key_length";
    pub const STATE_ERROR_KEY_NOT_SEALED: &str = "state.error.key_not_sealed";
    pub const STATE_ERROR_NOT_UTF8: &str = "state.error.not_utf8";
    pub const STATE_ERROR_PARSE: &str = "state.error.parse";

    // client
    pub const CLIENT_ERROR_BOOLEAN_INVALID: &str = "client.error.boolean_invalid";
    pub const CLIENT_ERROR_CREATE_TRAY_WINDOW: &str = "client.error.create_tray_window";
    pub const CLIENT_ERROR_CURRENT_DIR: &str = "client.error.current_dir";
    pub const CLIENT_ERROR_ENGINE_NOT_RUNNING: &str = "client.error.engine_not_running";
    pub const CLIENT_ERROR_EXE_PATH: &str = "client.error.exe_path";
    pub const CLIENT_ERROR_EXTRA_ARGS: &str = "client.error.extra_args";
    pub const CLIENT_ERROR_FOLLOW_LOG: &str = "client.error.follow_log";
    pub const CLIENT_ERROR_INSTALL_DIR: &str = "client.error.install_dir";
    pub const CLIENT_ERROR_INSTANCE_RUNNING: &str = "client.error.instance_running";
    pub const CLIENT_ERROR_LOAD_TRAY_ICON: &str = "client.error.load_tray_icon";
    pub const CLIENT_ERROR_MISSING_VALUE: &str = "client.error.missing_value";
    pub const CLIENT_ERROR_NOT_REGISTERED: &str = "client.error.not_registered";
    pub const CLIENT_ERROR_OPEN_APP_DIR: &str = "client.error.open_app_dir";
    pub const CLIENT_ERROR_OPEN_PANEL: &str = "client.error.open_panel";
    pub const CLIENT_ERROR_REGISTER_CONSOLE_HANDLER: &str = "client.error.register_console_handler";
    pub const CLIENT_ERROR_REGISTER_SIGNALS: &str = "client.error.register_signals";
    pub const CLIENT_ERROR_REGISTER_TRAY_CLASS: &str = "client.error.register_tray_class";
    pub const CLIENT_ERROR_SIGNAL_CHANNEL: &str = "client.error.signal_channel";
    pub const CLIENT_ERROR_START_EXPLORER: &str = "client.error.start_explorer";
    pub const CLIENT_ERROR_START_PANEL: &str = "client.error.start_panel";
    pub const CLIENT_ERROR_START_SIGNAL_THREAD: &str = "client.error.start_signal_thread";
    pub const CLIENT_ERROR_START_TRAY_THREAD: &str = "client.error.start_tray_thread";
    pub const CLIENT_ERROR_UNKNOWN_OPTION: &str = "client.error.unknown_option";
    pub const CLIENT_ERROR_WRITE_BOOT_MARKER: &str = "client.error.write_boot_marker";
    pub const CLIENT_ERROR_WRITE_TRAY_ICON: &str = "client.error.write_tray_icon";
    pub const CLIENT_MENU_AUTOSTART: &str = "client.menu.autostart";
    pub const CLIENT_MENU_OPEN_APP_DIR: &str = "client.menu.open_app_dir";
    pub const CLIENT_MENU_OPEN_PANEL: &str = "client.menu.open_panel";
    pub const CLIENT_MENU_QUIT: &str = "client.menu.quit";
    pub const CLIENT_TEXT_ADDRESS: &str = "client.text.address";
    pub const CLIENT_TEXT_ADDRESS_UNCONNECTED: &str = "client.text.address_unconnected";
    pub const CLIENT_TEXT_EXITING: &str = "client.text.exiting";
    pub const CLIENT_TEXT_HEADLESS_RUNNING: &str = "client.text.headless_running";
    pub const CLIENT_TEXT_IDENTITY_LOST: &str = "client.text.identity_lost";
    pub const CLIENT_TEXT_INSTANCE_RUNNING: &str = "client.text.instance_running";
    pub const CLIENT_TEXT_NO_DESKTOP: &str = "client.text.no_desktop";
    pub const CLIENT_TEXT_NO_TRAY: &str = "client.text.no_tray";
    pub const CLIENT_TEXT_PANEL_URL: &str = "client.text.panel_url";
    pub const CLIENT_TEXT_UPDATE_DOWNLOADING: &str = "client.text.update_downloading";
    pub const CLIENT_TEXT_UPDATE_READY: &str = "client.text.update_ready";
    pub const CLIENT_TEXT_UPDATE_READY_VERSION: &str = "client.text.update_ready_version";
    pub const CLIENT_TEXT_USAGE_BOOT_CHECK: &str = "client.text.usage_boot_check";
    pub const CLIENT_TEXT_USAGE_BUILD_VERSION: &str = "client.text.usage_build_version";
    pub const CLIENT_TEXT_USAGE_CORE: &str = "client.text.usage_core";
    pub const CLIENT_TEXT_USAGE_DISABLE_AUTO_UPDATE: &str = "client.text.usage_disable_auto_update";
    pub const CLIENT_TEXT_USAGE_HEAD: &str = "client.text.usage_head";
    pub const CLIENT_TEXT_USAGE_HEADLESS: &str = "client.text.usage_headless";
    pub const CLIENT_TEXT_USAGE_HELP: &str = "client.text.usage_help";
    pub const CLIENT_TEXT_USAGE_NAME: &str = "client.text.usage_name";
    pub const CLIENT_TEXT_USAGE_ROOT: &str = "client.text.usage_root";
    pub const CLIENT_TEXT_USAGE_SERVER: &str = "client.text.usage_server";
    pub const CLIENT_TEXT_USAGE_TOKEN: &str = "client.text.usage_token";
    pub const CLIENT_TEXT_USAGE_VERSION: &str = "client.text.usage_version";

    // panel
    pub const PANEL_ERROR_LISTEN: &str = "panel.error.listen";
    pub const PANEL_ERROR_SERIALIZE_RESPONSE: &str = "panel.error.serialize_response";
    pub const PANEL_TEXT_ENGINE_NOT_RUNNING: &str = "panel.text.engine_not_running";

    // launcher
    pub const LAUNCHER_ERROR_BIN_OLD_REMAINS: &str = "launcher.error.bin_old_remains";
    pub const LAUNCHER_ERROR_BOOT_TIMEOUT: &str = "launcher.error.boot_timeout";
    pub const LAUNCHER_ERROR_CALL_CURL: &str = "launcher.error.call_curl";
    pub const LAUNCHER_ERROR_CLIENT_EXITED_IMMEDIATELY: &str =
        "launcher.error.client_exited_immediately";
    pub const LAUNCHER_ERROR_CLIENT_MISSING: &str = "launcher.error.client_missing";
    pub const LAUNCHER_ERROR_CREATE_BIN: &str = "launcher.error.create_bin";
    pub const LAUNCHER_ERROR_CREATE_FILE: &str = "launcher.error.create_file";
    pub const LAUNCHER_ERROR_DECOMPRESS_PACKAGE: &str = "launcher.error.decompress_package";
    pub const LAUNCHER_ERROR_DIGEST_MISMATCH: &str = "launcher.error.digest_mismatch";
    pub const LAUNCHER_ERROR_DOWNLOAD: &str = "launcher.error.download";
    pub const LAUNCHER_ERROR_DOWNLOAD_CURL: &str = "launcher.error.download_curl";
    pub const LAUNCHER_ERROR_EXE_MISSING: &str = "launcher.error.exe_missing";
    pub const LAUNCHER_ERROR_EXE_PATH: &str = "launcher.error.exe_path";
    pub const LAUNCHER_ERROR_FAILURES_EXHAUSTED: &str = "launcher.error.failures_exhausted";
    pub const LAUNCHER_ERROR_FILE_AND_URL: &str = "launcher.error.file_and_url";
    pub const LAUNCHER_ERROR_FILE_DIGEST_MISMATCH: &str = "launcher.error.file_digest_mismatch";
    pub const LAUNCHER_ERROR_ILLEGAL_PATH: &str = "launcher.error.illegal_path";
    pub const LAUNCHER_ERROR_INSTALL_AND_RELOAD: &str = "launcher.error.install_and_reload";
    pub const LAUNCHER_ERROR_INSTALL_DIR: &str = "launcher.error.install_dir";
    pub const LAUNCHER_ERROR_INSTALL_ONLY_FLAGS: &str = "launcher.error.install_only_flags";
    pub const LAUNCHER_ERROR_MISSING_FILE: &str = "launcher.error.missing_file";
    pub const LAUNCHER_ERROR_MISSING_VALUE: &str = "launcher.error.missing_value";
    pub const LAUNCHER_ERROR_MOVE_OLD_BIN: &str = "launcher.error.move_old_bin";
    pub const LAUNCHER_ERROR_NO_CHILD: &str = "launcher.error.no_child";
    pub const LAUNCHER_ERROR_NO_VERSION_JSON: &str = "launcher.error.no_version_json";
    pub const LAUNCHER_ERROR_NO_VERSION_JSON_SUPERVISOR: &str =
        "launcher.error.no_version_json_supervisor";
    pub const LAUNCHER_ERROR_NOT_NEWER: &str = "launcher.error.not_newer";
    pub const LAUNCHER_ERROR_OPEN_PACKAGE: &str = "launcher.error.open_package";
    pub const LAUNCHER_ERROR_PARSE_VERSION_JSON: &str = "launcher.error.parse_version_json";
    pub const LAUNCHER_ERROR_READ_ENTRY_NAME: &str = "launcher.error.read_entry_name";
    pub const LAUNCHER_ERROR_READ_FILE: &str = "launcher.error.read_file";
    pub const LAUNCHER_ERROR_READ_PACKAGE: &str = "launcher.error.read_package";
    pub const LAUNCHER_ERROR_READ_PACKAGE_ENTRY: &str = "launcher.error.read_package_entry";
    pub const LAUNCHER_ERROR_READ_VERSION_JSON: &str = "launcher.error.read_version_json";
    pub const LAUNCHER_ERROR_RELOAD_OLD: &str = "launcher.error.reload_old";
    pub const LAUNCHER_ERROR_ROLLBACK: &str = "launcher.error.rollback";
    pub const LAUNCHER_ERROR_START_CLIENT: &str = "launcher.error.start_client";
    pub const LAUNCHER_ERROR_START_CLIENT_ADMIN: &str = "launcher.error.start_client_admin";
    pub const LAUNCHER_ERROR_UNKNOWN_OPTION: &str = "launcher.error.unknown_option";
    pub const LAUNCHER_ERROR_UNPACK: &str = "launcher.error.unpack";
    pub const LAUNCHER_ERROR_WAIT_CLIENT: &str = "launcher.error.wait_client";
    pub const LAUNCHER_ERROR_WAIT_CLIENT_EXIT: &str = "launcher.error.wait_client_exit";
    pub const LAUNCHER_ERROR_WAIT_EXIT: &str = "launcher.error.wait_exit";
    pub const LAUNCHER_ERROR_WAIT_EXIT_TIMEOUT: &str = "launcher.error.wait_exit_timeout";
    pub const LAUNCHER_ERROR_WAIT_FOR_INVALID: &str = "launcher.error.wait_for_invalid";
    pub const LAUNCHER_ERROR_WRITE_FILE: &str = "launcher.error.write_file";
    pub const LAUNCHER_TEXT_BOOT_CONFIRMED: &str = "launcher.text.boot_confirmed";
    pub const LAUNCHER_TEXT_BOOT_FAILED_ROLLBACK: &str = "launcher.text.boot_failed_rollback";
    pub const LAUNCHER_TEXT_BOOT_OK: &str = "launcher.text.boot_ok";
    pub const LAUNCHER_TEXT_CLIENT_LEFT_UPDATE: &str = "launcher.text.client_left_update";
    pub const LAUNCHER_TEXT_DISCARD_NOT_NEWER: &str = "launcher.text.discard_not_newer";
    pub const LAUNCHER_TEXT_DOWNLOAD: &str = "launcher.text.download";
    pub const LAUNCHER_TEXT_FAILED: &str = "launcher.text.failed";
    pub const LAUNCHER_TEXT_INSTALL_FAILED: &str = "launcher.text.install_failed";
    pub const LAUNCHER_TEXT_INSTALL_FAILED_KEEP_RUNNING: &str =
        "launcher.text.install_failed_keep_running";
    pub const LAUNCHER_TEXT_INSTALL_UPDATE: &str = "launcher.text.install_update";
    pub const LAUNCHER_TEXT_INSTALLED_FOR_SYSTEMD: &str = "launcher.text.installed_for_systemd";
    pub const LAUNCHER_TEXT_INSTALLING: &str = "launcher.text.installing";
    pub const LAUNCHER_TEXT_KILL_CLIENT: &str = "launcher.text.kill_client";
    pub const LAUNCHER_TEXT_PACKAGE_DIGEST: &str = "launcher.text.package_digest";
    pub const LAUNCHER_TEXT_RECOVER_STALE: &str = "launcher.text.recover_stale";
    pub const LAUNCHER_TEXT_RELOADED: &str = "launcher.text.reloaded";
    pub const LAUNCHER_TEXT_START_CLIENT: &str = "launcher.text.start_client";
    pub const LAUNCHER_TEXT_START_FAILED: &str = "launcher.text.start_failed";
    pub const LAUNCHER_TEXT_STARTED_NEW: &str = "launcher.text.started_new";
    pub const LAUNCHER_TEXT_STARTING: &str = "launcher.text.starting";
    pub const LAUNCHER_TEXT_UAC_CANCELLED: &str = "launcher.text.uac_cancelled";
    pub const LAUNCHER_TEXT_UPDATE_UNUSABLE: &str = "launcher.text.update_unusable";
    pub const LAUNCHER_TEXT_USAGE_INSTALL_HEAD: &str = "launcher.text.usage_install_head";
    pub const LAUNCHER_TEXT_USAGE_INSTALL_LINE: &str = "launcher.text.usage_install_line";
    pub const LAUNCHER_TEXT_USAGE_LAUNCH: &str = "launcher.text.usage_launch";
    pub const LAUNCHER_TEXT_USAGE_LAUNCH_LINE: &str = "launcher.text.usage_launch_line";
    pub const LAUNCHER_TEXT_USAGE_URL_HEAD: &str = "launcher.text.usage_url_head";
    pub const LAUNCHER_TEXT_USAGE_URL_LINE: &str = "launcher.text.usage_url_line";
    pub const LAUNCHER_TEXT_VERIFIED: &str = "launcher.text.verified";
    pub const LAUNCHER_TEXT_WAIT_PROCESS: &str = "launcher.text.wait_process";

    // engine
    pub const ENGINE_TEXT_AUTOSTART_CREATE_FAILED: &str = "engine.text.autostart_create_failed";
    pub const ENGINE_TEXT_AUTOSTART_DELETE_FAILED: &str = "engine.text.autostart_delete_failed";
    pub const ENGINE_TEXT_AUTOSTART_LAUNCHER_MISSING: &str =
        "engine.text.autostart_launcher_missing";
    pub const ENGINE_TEXT_AUTOSTART_LAUNCHER_PATH: &str = "engine.text.autostart_launcher_path";
    pub const ENGINE_TEXT_AUTOSTART_TASK_DESCRIPTION: &str =
        "engine.text.autostart_task_description";
    pub const ENGINE_TEXT_AUTOSTART_TEMP_DIR: &str = "engine.text.autostart_temp_dir";
    pub const ENGINE_TEXT_AUTOSTART_UNSUPPORTED: &str = "engine.text.autostart_unsupported";
    pub const ENGINE_TEXT_AUTOSTART_WRITE_XML: &str = "engine.text.autostart_write_xml";
    pub const ENGINE_TEXT_AUTH_REJECTED_RETRY: &str = "engine.text.auth_rejected_retry";
    pub const ENGINE_TEXT_CLEAR_DEVICE_FAILED: &str = "engine.text.clear_device_failed";
    pub const ENGINE_TEXT_CLOCK_SKEW: &str = "engine.text.clock_skew";
    pub const ENGINE_TEXT_CONFIG_DECODE_FAILED: &str = "engine.text.config_decode_failed";
    pub const ENGINE_TEXT_CONNECT_SERVER_FAILED: &str = "engine.text.connect_server_failed";
    pub const ENGINE_TEXT_CORE_NOT_RUNNING: &str = "engine.text.core_not_running";
    pub const ENGINE_TEXT_CORE_NOT_RUNNING_RESTARTING: &str =
        "engine.text.core_not_running_restarting";
    pub const ENGINE_TEXT_CORE_THREAD_CLOSED: &str = "engine.text.core_thread_closed";
    pub const ENGINE_TEXT_DELETE_CONFIG_FAILED: &str = "engine.text.delete_config_failed";
    pub const ENGINE_TEXT_DIGEST_MISMATCH: &str = "engine.text.digest_mismatch";
    pub const ENGINE_TEXT_DOWNLOAD_THREAD_FAILED: &str = "engine.text.download_thread_failed";
    pub const ENGINE_TEXT_EXITED_DETAIL: &str = "engine.text.exited_detail";
    pub const ENGINE_TEXT_IDENTITY_INVALID_REGISTER: &str = "engine.text.identity_invalid_register";
    pub const ENGINE_TEXT_LAUNCHER_MISSING: &str = "engine.text.launcher_missing";
    pub const ENGINE_TEXT_LAUNCHER_START_FAILED: &str = "engine.text.launcher_start_failed";
    pub const ENGINE_TEXT_LOG_UNAVAILABLE: &str = "engine.text.log_unavailable";
    pub const ENGINE_TEXT_NO_CONFIG_LOCAL: &str = "engine.text.no_config_local";
    pub const ENGINE_TEXT_NO_CREDENTIAL: &str = "engine.text.no_credential";
    pub const ENGINE_TEXT_NO_DIGEST_REFUSE: &str = "engine.text.no_digest_refuse";
    pub const ENGINE_TEXT_NO_SERVER_FOR_DOWNLOAD: &str = "engine.text.no_server_for_download";
    pub const ENGINE_TEXT_NOT_REGISTERED: &str = "engine.text.not_registered";
    pub const ENGINE_TEXT_NOT_REGISTERED_TOKEN: &str = "engine.text.not_registered_token";
    pub const ENGINE_TEXT_NOTHING_TO_INSTALL: &str = "engine.text.nothing_to_install";
    pub const ENGINE_TEXT_PACKAGE_NOT_LOCAL: &str = "engine.text.package_not_local";
    pub const ENGINE_TEXT_PERSIST_FAILED: &str = "engine.text.persist_failed";
    pub const ENGINE_TEXT_READ_CREDENTIAL_FAILED: &str = "engine.text.read_credential_failed";
    pub const ENGINE_TEXT_READ_DEVICE_KEY_FAILED: &str = "engine.text.read_device_key_failed";
    pub const ENGINE_TEXT_READ_DOWNLOAD_FAILED: &str = "engine.text.read_download_failed";
    pub const ENGINE_TEXT_READ_NODE_KEY_FAILED: &str = "engine.text.read_node_key_failed";
    pub const ENGINE_TEXT_REGISTER_CONNECT_FAILED: &str = "engine.text.register_connect_failed";
    pub const ENGINE_TEXT_REGISTER_FAILED: &str = "engine.text.register_failed";
    pub const ENGINE_TEXT_REGISTER_REJECTED: &str = "engine.text.register_rejected";
    pub const ENGINE_TEXT_REMOVE_OLD_CREDENTIAL_FAILED: &str =
        "engine.text.remove_old_credential_failed";
    pub const ENGINE_TEXT_RPC_PORTAL_INVALID: &str = "engine.text.rpc_portal_invalid";
    pub const ENGINE_TEXT_SAVE_REGISTER_FAILED: &str = "engine.text.save_register_failed";
    pub const ENGINE_TEXT_SAVE_SETTINGS_FAILED: &str = "engine.text.save_settings_failed";
    pub const ENGINE_TEXT_SCHTASKS_EXIT: &str = "engine.text.schtasks_exit";
    pub const ENGINE_TEXT_SCHTASKS_EXIT_OUTPUT: &str = "engine.text.schtasks_exit_output";
    pub const ENGINE_TEXT_SCHTASKS_SPAWN: &str = "engine.text.schtasks_spawn";
    pub const ENGINE_TEXT_SERVER_URL_INVALID: &str = "engine.text.server_url_invalid";
    pub const ENGINE_TEXT_SERVER_URL_NO_HOST: &str = "engine.text.server_url_no_host";
    pub const ENGINE_TEXT_SERVER_URL_REQUIRED: &str = "engine.text.server_url_required";
    pub const ENGINE_TEXT_SERVER_URL_SCHEME: &str = "engine.text.server_url_scheme";
    pub const ENGINE_TEXT_SET_AUTOSTART_FAILED: &str = "engine.text.set_autostart_failed";
    pub const ENGINE_TEXT_STOP_CORE_TIMEOUT: &str = "engine.text.stop_core_timeout";
    pub const ENGINE_TEXT_TOKEN_INVALID: &str = "engine.text.token_invalid";
    pub const ENGINE_TEXT_UPDATE_DOWNLOADING: &str = "engine.text.update_downloading";
    pub const ENGINE_TEXT_UPDATE_IN_PROGRESS: &str = "engine.text.update_in_progress";
    pub const ENGINE_TEXT_VERSION_INVALID: &str = "engine.text.version_invalid";
    pub const ENGINE_TEXT_WRITE_CONFIG_FAILED: &str = "engine.text.write_config_failed";
    pub const ENGINE_LOG_AUTH_FAILURES_LIMIT: &str = "engine.log.auth_failures_limit";
    pub const ENGINE_LOG_AUTO_APPLY: &str = "engine.log.auto_apply";
    pub const ENGINE_LOG_AUTO_APPLY_FAILED: &str = "engine.log.auto_apply_failed";
    pub const ENGINE_LOG_AUTOSTART_DISABLED: &str = "engine.log.autostart_disabled";
    pub const ENGINE_LOG_AUTOSTART_ENABLED: &str = "engine.log.autostart_enabled";
    pub const ENGINE_LOG_CLEAN_TEMP_FILE: &str = "engine.log.clean_temp_file";
    pub const ENGINE_LOG_CLEAN_TEMP_FILE_FAILED: &str = "engine.log.clean_temp_file_failed";
    pub const ENGINE_LOG_CLEAR_DEVICE_FAILED: &str = "engine.log.clear_device_failed";
    pub const ENGINE_LOG_CLIENT_STARTED: &str = "engine.log.client_started";
    pub const ENGINE_LOG_CONFIG_APPLIED: &str = "engine.log.config_applied";
    pub const ENGINE_LOG_CONFIG_CHANGED_SYNC: &str = "engine.log.config_changed_sync";
    pub const ENGINE_LOG_CONFIG_CHANGED_WITHOUT_BODY: &str =
        "engine.log.config_changed_without_body";
    pub const ENGINE_LOG_CONFIG_UNCHANGED_CORE_STARTED: &str =
        "engine.log.config_unchanged_core_started";
    pub const ENGINE_LOG_CONNECT_FAILED: &str = "engine.log.connect_failed";
    pub const ENGINE_LOG_CONNECTED_SERVER: &str = "engine.log.connected_server";
    pub const ENGINE_LOG_CORE_EXITED: &str = "engine.log.core_exited";
    pub const ENGINE_LOG_CORE_READY: &str = "engine.log.core_ready";
    pub const ENGINE_LOG_CORE_REVIVED: &str = "engine.log.core_revived";
    pub const ENGINE_LOG_CORE_START_FAILED: &str = "engine.log.core_start_failed";
    pub const ENGINE_LOG_CORE_START_FAILED_ATTEMPT: &str = "engine.log.core_start_failed_attempt";
    pub const ENGINE_LOG_CORE_STARTED_FROM_DISK: &str = "engine.log.core_started_from_disk";
    pub const ENGINE_LOG_CREDENTIAL_RECEIVED: &str = "engine.log.credential_received";
    pub const ENGINE_LOG_CREDENTIAL_RECEIVED_RESTART: &str =
        "engine.log.credential_received_restart";
    pub const ENGINE_LOG_DOWNLOAD_FAILED: &str = "engine.log.download_failed";
    pub const ENGINE_LOG_DOWNLOAD_STARTED: &str = "engine.log.download_started";
    pub const ENGINE_LOG_DOWNLOADED_MANUAL_INSTALL: &str = "engine.log.downloaded_manual_install";
    pub const ENGINE_LOG_HANDOFF_LAUNCHER: &str = "engine.log.handoff_launcher";
    pub const ENGINE_LOG_HEARTBEAT_FAILED: &str = "engine.log.heartbeat_failed";
    pub const ENGINE_LOG_HEARTBEAT_RECOVERED: &str = "engine.log.heartbeat_recovered";
    pub const ENGINE_LOG_IDENTITY_INVALID_CLEARED: &str = "engine.log.identity_invalid_cleared";
    pub const ENGINE_LOG_IGNORE_INVALID_VERSION: &str = "engine.log.ignore_invalid_version";
    pub const ENGINE_LOG_IGNORE_NO_DIGEST: &str = "engine.log.ignore_no_digest";
    pub const ENGINE_LOG_IGNORE_NOT_NEWER: &str = "engine.log.ignore_not_newer";
    pub const ENGINE_LOG_INSTALL_STARTED: &str = "engine.log.install_started";
    pub const ENGINE_LOG_LAUNCHER_CHMOD_FAILED: &str = "engine.log.launcher_chmod_failed";
    pub const ENGINE_LOG_LAUNCHER_DIGEST_MISMATCH: &str = "engine.log.launcher_digest_mismatch";
    pub const ENGINE_LOG_LAUNCHER_DOWNLOAD: &str = "engine.log.launcher_download";
    pub const ENGINE_LOG_LAUNCHER_DOWNLOAD_FAILED: &str = "engine.log.launcher_download_failed";
    pub const ENGINE_LOG_LAUNCHER_REPLACE_FAILED: &str = "engine.log.launcher_replace_failed";
    pub const ENGINE_LOG_LAUNCHER_THREAD_FAILED: &str = "engine.log.launcher_thread_failed";
    pub const ENGINE_LOG_LAUNCHER_UPDATE_FAILED: &str = "engine.log.launcher_update_failed";
    pub const ENGINE_LOG_LAUNCHER_UPDATED: &str = "engine.log.launcher_updated";
    pub const ENGINE_LOG_LAUNCHER_VERIFY_FAILED: &str = "engine.log.launcher_verify_failed";
    pub const ENGINE_LOG_LOCAL_CONFIG_DELETED: &str = "engine.log.local_config_deleted";
    pub const ENGINE_LOG_LOCAL_STATE_READ_FAILED: &str = "engine.log.local_state_read_failed";
    pub const ENGINE_LOG_READ_AUTOSTART_FAILED: &str = "engine.log.read_autostart_failed";
    pub const ENGINE_LOG_RECONNECT_MANUAL: &str = "engine.log.reconnect_manual";
    pub const ENGINE_LOG_REDOWNLOAD: &str = "engine.log.redownload";
    pub const ENGINE_LOG_REDOWNLOAD_FOR_INSTALL: &str = "engine.log.redownload_for_install";
    pub const ENGINE_LOG_REGISTERED: &str = "engine.log.registered";
    pub const ENGINE_LOG_SAVE_CREDENTIAL_FAILED: &str = "engine.log.save_credential_failed";
    pub const ENGINE_LOG_SETTINGS_SAVED: &str = "engine.log.settings_saved";
    pub const ENGINE_LOG_SKIP_FAILED_VERSION: &str = "engine.log.skip_failed_version";
    pub const ENGINE_LOG_STOP_CORE_FAILED: &str = "engine.log.stop_core_failed";
    pub const ENGINE_LOG_UPDATE_READY_MANUAL: &str = "engine.log.update_ready_manual";
    pub const ENGINE_LOG_UPDATE_URL_NOT_ABSOLUTE: &str = "engine.log.update_url_not_absolute";
}

/// 查一条文案。id 不在清单里时原样返回 id，不 panic。
pub fn get(id: &'static str) -> &'static str {
    chinese().get(id).copied().unwrap_or(id)
}

/// 按模板替换 `{0}`、`{1}`……，返回结果。
///
/// 参数个数与模板占位符对不上时，DEBUG 构建由 `debug_assert!` 报出；发布构建
/// 按缺省替换成空串，不 panic。
pub fn format(id: &'static str, args: &[&dyn core::fmt::Display]) -> String {
    let (text, counts_match) = render(get(id), args);
    debug_assert!(counts_match, "mesh-text: {id} 的占位符与参数个数不一致");
    text
}

fn chinese() -> &'static HashMap<&'static str, &'static str> {
    CHINESE.get_or_init(|| {
        parse(CHINESE_LANG).unwrap_or_else(|err| panic!("mesh-text: 内嵌中文清单解析失败: {err:?}"))
    })
}

/// 纯粹的替换：返回（结果, 占位符编号与参数个数是否一致）。
fn render(template: &str, args: &[&dyn core::fmt::Display]) -> (String, bool) {
    let mut out = String::with_capacity(template.len());
    let mut highest: Option<usize> = None;
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && after.as_bytes().get(digits) == Some(&b'}') {
            let index = after[..digits].parse::<usize>().unwrap_or(usize::MAX);
            if let Some(arg) = args.get(index) {
                let _ = write!(out, "{arg}");
            }
            highest = Some(highest.map_or(index, |seen| seen.max(index)));
            rest = &after[digits + 1..];
        } else {
            // 清单里不会出现这种形态（解析阶段就拒绝）；真出现了按字面量输出。
            out.push('{');
            rest = after;
        }
    }
    out.push_str(rest);
    let expected = highest.map_or(0, |index| index + 1);
    (out, expected == args.len())
}

/// 允许的文案类别。
const CLASSES: [&str; 5] = ["text", "log", "error", "menu", "notice"];

/// 一行里 key/value 不合法时的原因。行号从 1 起。
#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    /// 一行既不是空行/注释，也没有 `=`。
    MissingEquals { line: usize, text: String },
    /// key 不是 `<crate>.<类>.<标识>`，或含 `[a-z0-9_.]` 以外的字符。
    InvalidKey { line: usize, key: String },
    /// value 去掉首尾空白后为空。
    EmptyValue { line: usize, key: String },
    /// 同一个 key 出现两次。
    DuplicateKey { line: usize, key: String },
    /// value 里的花括号不是 `{n}`，或编号不是从 0 连续。
    BadPlaceholder {
        line: usize,
        key: String,
        value: String,
    },
}

/// 解析一份清单。key 与 value 各自 trim，第一个 `=` 是分隔符。
fn parse(text: &str) -> Result<HashMap<&str, &str>, ParseError> {
    let mut table = HashMap::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            return Err(ParseError::MissingEquals {
                line,
                text: trimmed.to_string(),
            });
        };
        let key = key.trim();
        let value = value.trim();
        if !valid_key(key) {
            return Err(ParseError::InvalidKey {
                line,
                key: key.to_string(),
            });
        }
        if value.is_empty() {
            return Err(ParseError::EmptyValue {
                line,
                key: key.to_string(),
            });
        }
        if !valid_placeholders(value) {
            return Err(ParseError::BadPlaceholder {
                line,
                key: key.to_string(),
                value: value.to_string(),
            });
        }
        if table.insert(key, value).is_some() {
            return Err(ParseError::DuplicateKey {
                line,
                key: key.to_string(),
            });
        }
    }
    Ok(table)
}

/// key 只允许 `[a-z0-9_.]`，且必须是非空的三段点分、类名在白名单里。
fn valid_key(key: &str) -> bool {
    if key.is_empty()
        || !key.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'_'
        })
    {
        return false;
    }
    let mut parts = key.split('.');
    let (Some(crate_name), Some(class), Some(name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !crate_name.is_empty() && CLASSES.contains(&class) && !name.is_empty()
}

/// value 里的花括号必须都是 `{n}`，且用到的编号恰好是 0..k。
fn valid_placeholders(value: &str) -> bool {
    let mut used = Vec::new();
    let mut rest = value;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return false;
        }
        let after = &rest[open + 1..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 || after.as_bytes().get(digits) != Some(&b'}') {
            return false;
        }
        let Ok(index) = after[..digits].parse::<usize>() else {
            return false;
        };
        used.push(index);
        rest = &after[digits + 1..];
    }
    used.sort_unstable();
    used.dedup();
    used.iter()
        .enumerate()
        .all(|(index, &value)| index == value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// `id` 模块里的全部常量。Rust 没有反射，这张表只能手写；
    /// `ids_and_the_chinese_table_are_one_to_one` 保证它与清单同步。
    const ALL_IDS: &[&str] = &[
        // core
        id::CORE_ERROR_ASSIGN_JOB,
        id::CORE_ERROR_ASSIGN_JOB_LIFECYCLE,
        id::CORE_ERROR_CORE_NOT_FOUND,
        id::CORE_ERROR_CREATE_JOB,
        id::CORE_ERROR_IDENTITY_MISMATCH,
        id::CORE_ERROR_LAUNCH,
        id::CORE_ERROR_LINE_TOO_LONG,
        id::CORE_ERROR_OPEN_PROCESS,
        id::CORE_ERROR_OUTPUT_INTERRUPTED,
        id::CORE_ERROR_PROCESS_EXITED,
        id::CORE_ERROR_PROCESS_STUCK,
        id::CORE_ERROR_READY_TIMEOUT,
        id::CORE_ERROR_SET_JOB,
        id::CORE_ERROR_SIGNAL,
        id::CORE_ERROR_STOP_FAILED,
        id::CORE_ERROR_TASKKILL_CALL,
        id::CORE_ERROR_TASKKILL_STATUS,
        id::CORE_ERROR_WAIT_EXIT,
        id::CORE_ERROR_WAIT_FAILED,
        id::CORE_TEXT_ABNORMAL_EXIT,
        id::CORE_TEXT_EXIT_CODE,
        id::CORE_TEXT_KILLED_BY_SIGNAL,
        id::CORE_TEXT_NODE_ONLINE,
        id::CORE_TEXT_PROCESS_EXITED,
        id::CORE_TEXT_TIMEOUT,
        // etrpc
        id::ETRPC_ERROR_CONNECT,
        id::ETRPC_ERROR_EXECUTION_FAILED,
        id::ETRPC_ERROR_FRAME,
        id::ETRPC_ERROR_FRAME_LENGTH,
        id::ETRPC_ERROR_METHOD_NOT_FOUND,
        id::ETRPC_ERROR_MISSING_PIECE,
        id::ETRPC_ERROR_PACKET_MALFORMED,
        id::ETRPC_ERROR_PAYLOAD_TOO_LARGE,
        id::ETRPC_ERROR_PIECE_COUNT,
        id::ETRPC_ERROR_PROTOCOL,
        id::ETRPC_ERROR_REQUEST_DECODE,
        id::ETRPC_ERROR_RESPONSE_ENCODE,
        id::ETRPC_ERROR_RESPONSE_TOO_LARGE,
        id::ETRPC_ERROR_RPC_FAILED,
        id::ETRPC_ERROR_RPC_NO_REASON,
        id::ETRPC_ERROR_RPC_TIMEOUT,
        id::ETRPC_ERROR_RPC_UNKNOWN_TYPE,
        id::ETRPC_ERROR_RPC_UNPARSABLE,
        id::ETRPC_ERROR_SERVICE_NOT_FOUND,
        id::ETRPC_ERROR_TIMEOUT,
        id::ETRPC_ERROR_UNEXPECTED_PACKET,
        // install
        id::INSTALL_ERROR_ALREADY_RUNNING,
        id::INSTALL_ERROR_MANIFEST_READ,
        id::INSTALL_ERROR_NO_PARENT_DIR,
        id::INSTALL_ERROR_TEMP_NAME_COLLISION,
        id::INSTALL_ERROR_UNSUPPORTED_PLATFORM,
        id::INSTALL_ERROR_VERSION_INVALID,
        // remote
        id::REMOTE_ERROR_API,
        id::REMOTE_ERROR_BODY_TOO_LARGE,
        id::REMOTE_ERROR_CONFIG_DECODE,
        id::REMOTE_ERROR_CONFIG_NOT_UTF8,
        id::REMOTE_ERROR_CREATE_FILE,
        id::REMOTE_ERROR_DECODE,
        id::REMOTE_ERROR_READ_RESPONSE,
        id::REMOTE_ERROR_TRANSPORT,
        id::REMOTE_ERROR_WRITE_FILE,
        // state
        id::STATE_ERROR_CREDENTIAL_CIPHER_NOT_BASE64,
        id::STATE_ERROR_CREDENTIAL_DPAPI_UNREADABLE,
        id::STATE_ERROR_CREDENTIAL_NOT_BASE64,
        id::STATE_ERROR_CREDENTIAL_PLAINTEXT,
        id::STATE_ERROR_EMPTY_CREDENTIAL_SECRET,
        id::STATE_ERROR_EMPTY_ROOT,
        id::STATE_ERROR_IO,
        id::STATE_ERROR_KEY_LENGTH,
        id::STATE_ERROR_KEY_NOT_SEALED,
        id::STATE_ERROR_NOT_UTF8,
        id::STATE_ERROR_PARSE,
        // client
        id::CLIENT_ERROR_BOOLEAN_INVALID,
        id::CLIENT_ERROR_CREATE_TRAY_WINDOW,
        id::CLIENT_ERROR_CURRENT_DIR,
        id::CLIENT_ERROR_ENGINE_NOT_RUNNING,
        id::CLIENT_ERROR_EXE_PATH,
        id::CLIENT_ERROR_EXTRA_ARGS,
        id::CLIENT_ERROR_FOLLOW_LOG,
        id::CLIENT_ERROR_INSTALL_DIR,
        id::CLIENT_ERROR_INSTANCE_RUNNING,
        id::CLIENT_ERROR_LOAD_TRAY_ICON,
        id::CLIENT_ERROR_MISSING_VALUE,
        id::CLIENT_ERROR_NOT_REGISTERED,
        id::CLIENT_ERROR_OPEN_APP_DIR,
        id::CLIENT_ERROR_OPEN_PANEL,
        id::CLIENT_ERROR_REGISTER_CONSOLE_HANDLER,
        id::CLIENT_ERROR_REGISTER_SIGNALS,
        id::CLIENT_ERROR_REGISTER_TRAY_CLASS,
        id::CLIENT_ERROR_SIGNAL_CHANNEL,
        id::CLIENT_ERROR_START_EXPLORER,
        id::CLIENT_ERROR_START_PANEL,
        id::CLIENT_ERROR_START_SIGNAL_THREAD,
        id::CLIENT_ERROR_START_TRAY_THREAD,
        id::CLIENT_ERROR_UNKNOWN_OPTION,
        id::CLIENT_ERROR_WRITE_BOOT_MARKER,
        id::CLIENT_ERROR_WRITE_TRAY_ICON,
        id::CLIENT_MENU_AUTOSTART,
        id::CLIENT_MENU_OPEN_APP_DIR,
        id::CLIENT_MENU_OPEN_PANEL,
        id::CLIENT_MENU_QUIT,
        id::CLIENT_TEXT_ADDRESS,
        id::CLIENT_TEXT_ADDRESS_UNCONNECTED,
        id::CLIENT_TEXT_EXITING,
        id::CLIENT_TEXT_HEADLESS_RUNNING,
        id::CLIENT_TEXT_IDENTITY_LOST,
        id::CLIENT_TEXT_INSTANCE_RUNNING,
        id::CLIENT_TEXT_NO_DESKTOP,
        id::CLIENT_TEXT_NO_TRAY,
        id::CLIENT_TEXT_PANEL_URL,
        id::CLIENT_TEXT_UPDATE_DOWNLOADING,
        id::CLIENT_TEXT_UPDATE_READY,
        id::CLIENT_TEXT_UPDATE_READY_VERSION,
        id::CLIENT_TEXT_USAGE_BOOT_CHECK,
        id::CLIENT_TEXT_USAGE_BUILD_VERSION,
        id::CLIENT_TEXT_USAGE_CORE,
        id::CLIENT_TEXT_USAGE_DISABLE_AUTO_UPDATE,
        id::CLIENT_TEXT_USAGE_HEAD,
        id::CLIENT_TEXT_USAGE_HEADLESS,
        id::CLIENT_TEXT_USAGE_HELP,
        id::CLIENT_TEXT_USAGE_NAME,
        id::CLIENT_TEXT_USAGE_ROOT,
        id::CLIENT_TEXT_USAGE_SERVER,
        id::CLIENT_TEXT_USAGE_TOKEN,
        id::CLIENT_TEXT_USAGE_VERSION,
        // panel
        id::PANEL_ERROR_LISTEN,
        id::PANEL_ERROR_SERIALIZE_RESPONSE,
        id::PANEL_TEXT_ENGINE_NOT_RUNNING,
        // launcher
        id::LAUNCHER_ERROR_BIN_OLD_REMAINS,
        id::LAUNCHER_ERROR_BOOT_TIMEOUT,
        id::LAUNCHER_ERROR_CALL_CURL,
        id::LAUNCHER_ERROR_CLIENT_EXITED_IMMEDIATELY,
        id::LAUNCHER_ERROR_CLIENT_MISSING,
        id::LAUNCHER_ERROR_CREATE_BIN,
        id::LAUNCHER_ERROR_CREATE_FILE,
        id::LAUNCHER_ERROR_DECOMPRESS_PACKAGE,
        id::LAUNCHER_ERROR_DIGEST_MISMATCH,
        id::LAUNCHER_ERROR_DOWNLOAD,
        id::LAUNCHER_ERROR_DOWNLOAD_CURL,
        id::LAUNCHER_ERROR_EXE_MISSING,
        id::LAUNCHER_ERROR_EXE_PATH,
        id::LAUNCHER_ERROR_FAILURES_EXHAUSTED,
        id::LAUNCHER_ERROR_FILE_AND_URL,
        id::LAUNCHER_ERROR_FILE_DIGEST_MISMATCH,
        id::LAUNCHER_ERROR_ILLEGAL_PATH,
        id::LAUNCHER_ERROR_INSTALL_AND_RELOAD,
        id::LAUNCHER_ERROR_INSTALL_DIR,
        id::LAUNCHER_ERROR_INSTALL_ONLY_FLAGS,
        id::LAUNCHER_ERROR_MISSING_FILE,
        id::LAUNCHER_ERROR_MISSING_VALUE,
        id::LAUNCHER_ERROR_MOVE_OLD_BIN,
        id::LAUNCHER_ERROR_NO_CHILD,
        id::LAUNCHER_ERROR_NO_VERSION_JSON,
        id::LAUNCHER_ERROR_NO_VERSION_JSON_SUPERVISOR,
        id::LAUNCHER_ERROR_NOT_NEWER,
        id::LAUNCHER_ERROR_OPEN_PACKAGE,
        id::LAUNCHER_ERROR_PARSE_VERSION_JSON,
        id::LAUNCHER_ERROR_READ_ENTRY_NAME,
        id::LAUNCHER_ERROR_READ_FILE,
        id::LAUNCHER_ERROR_READ_PACKAGE,
        id::LAUNCHER_ERROR_READ_PACKAGE_ENTRY,
        id::LAUNCHER_ERROR_READ_VERSION_JSON,
        id::LAUNCHER_ERROR_RELOAD_OLD,
        id::LAUNCHER_ERROR_ROLLBACK,
        id::LAUNCHER_ERROR_START_CLIENT,
        id::LAUNCHER_ERROR_START_CLIENT_ADMIN,
        id::LAUNCHER_ERROR_UNKNOWN_OPTION,
        id::LAUNCHER_ERROR_UNPACK,
        id::LAUNCHER_ERROR_WAIT_CLIENT,
        id::LAUNCHER_ERROR_WAIT_CLIENT_EXIT,
        id::LAUNCHER_ERROR_WAIT_EXIT,
        id::LAUNCHER_ERROR_WAIT_EXIT_TIMEOUT,
        id::LAUNCHER_ERROR_WAIT_FOR_INVALID,
        id::LAUNCHER_ERROR_WRITE_FILE,
        id::LAUNCHER_TEXT_BOOT_CONFIRMED,
        id::LAUNCHER_TEXT_BOOT_FAILED_ROLLBACK,
        id::LAUNCHER_TEXT_BOOT_OK,
        id::LAUNCHER_TEXT_CLIENT_LEFT_UPDATE,
        id::LAUNCHER_TEXT_DISCARD_NOT_NEWER,
        id::LAUNCHER_TEXT_DOWNLOAD,
        id::LAUNCHER_TEXT_FAILED,
        id::LAUNCHER_TEXT_INSTALL_FAILED,
        id::LAUNCHER_TEXT_INSTALL_FAILED_KEEP_RUNNING,
        id::LAUNCHER_TEXT_INSTALL_UPDATE,
        id::LAUNCHER_TEXT_INSTALLED_FOR_SYSTEMD,
        id::LAUNCHER_TEXT_INSTALLING,
        id::LAUNCHER_TEXT_KILL_CLIENT,
        id::LAUNCHER_TEXT_PACKAGE_DIGEST,
        id::LAUNCHER_TEXT_RECOVER_STALE,
        id::LAUNCHER_TEXT_RELOADED,
        id::LAUNCHER_TEXT_START_CLIENT,
        id::LAUNCHER_TEXT_START_FAILED,
        id::LAUNCHER_TEXT_STARTED_NEW,
        id::LAUNCHER_TEXT_STARTING,
        id::LAUNCHER_TEXT_UAC_CANCELLED,
        id::LAUNCHER_TEXT_UPDATE_UNUSABLE,
        id::LAUNCHER_TEXT_USAGE_INSTALL_HEAD,
        id::LAUNCHER_TEXT_USAGE_INSTALL_LINE,
        id::LAUNCHER_TEXT_USAGE_LAUNCH,
        id::LAUNCHER_TEXT_USAGE_LAUNCH_LINE,
        id::LAUNCHER_TEXT_USAGE_URL_HEAD,
        id::LAUNCHER_TEXT_USAGE_URL_LINE,
        id::LAUNCHER_TEXT_VERIFIED,
        id::LAUNCHER_TEXT_WAIT_PROCESS,
        // engine
        id::ENGINE_TEXT_AUTOSTART_CREATE_FAILED,
        id::ENGINE_TEXT_AUTOSTART_DELETE_FAILED,
        id::ENGINE_TEXT_AUTOSTART_LAUNCHER_MISSING,
        id::ENGINE_TEXT_AUTOSTART_LAUNCHER_PATH,
        id::ENGINE_TEXT_AUTOSTART_TASK_DESCRIPTION,
        id::ENGINE_TEXT_AUTOSTART_TEMP_DIR,
        id::ENGINE_TEXT_AUTOSTART_UNSUPPORTED,
        id::ENGINE_TEXT_AUTOSTART_WRITE_XML,
        id::ENGINE_TEXT_AUTH_REJECTED_RETRY,
        id::ENGINE_TEXT_CLEAR_DEVICE_FAILED,
        id::ENGINE_TEXT_CLOCK_SKEW,
        id::ENGINE_TEXT_CONFIG_DECODE_FAILED,
        id::ENGINE_TEXT_CONNECT_SERVER_FAILED,
        id::ENGINE_TEXT_CORE_NOT_RUNNING,
        id::ENGINE_TEXT_CORE_NOT_RUNNING_RESTARTING,
        id::ENGINE_TEXT_CORE_THREAD_CLOSED,
        id::ENGINE_TEXT_DELETE_CONFIG_FAILED,
        id::ENGINE_TEXT_DIGEST_MISMATCH,
        id::ENGINE_TEXT_DOWNLOAD_THREAD_FAILED,
        id::ENGINE_TEXT_EXITED_DETAIL,
        id::ENGINE_TEXT_IDENTITY_INVALID_REGISTER,
        id::ENGINE_TEXT_LAUNCHER_MISSING,
        id::ENGINE_TEXT_LAUNCHER_START_FAILED,
        id::ENGINE_TEXT_LOG_UNAVAILABLE,
        id::ENGINE_TEXT_NO_CONFIG_LOCAL,
        id::ENGINE_TEXT_NO_CREDENTIAL,
        id::ENGINE_TEXT_NO_DIGEST_REFUSE,
        id::ENGINE_TEXT_NO_SERVER_FOR_DOWNLOAD,
        id::ENGINE_TEXT_NOT_REGISTERED,
        id::ENGINE_TEXT_NOT_REGISTERED_TOKEN,
        id::ENGINE_TEXT_NOTHING_TO_INSTALL,
        id::ENGINE_TEXT_PACKAGE_NOT_LOCAL,
        id::ENGINE_TEXT_PERSIST_FAILED,
        id::ENGINE_TEXT_READ_CREDENTIAL_FAILED,
        id::ENGINE_TEXT_READ_DEVICE_KEY_FAILED,
        id::ENGINE_TEXT_READ_DOWNLOAD_FAILED,
        id::ENGINE_TEXT_READ_NODE_KEY_FAILED,
        id::ENGINE_TEXT_REGISTER_CONNECT_FAILED,
        id::ENGINE_TEXT_REGISTER_FAILED,
        id::ENGINE_TEXT_REGISTER_REJECTED,
        id::ENGINE_TEXT_REMOVE_OLD_CREDENTIAL_FAILED,
        id::ENGINE_TEXT_RPC_PORTAL_INVALID,
        id::ENGINE_TEXT_SAVE_REGISTER_FAILED,
        id::ENGINE_TEXT_SAVE_SETTINGS_FAILED,
        id::ENGINE_TEXT_SCHTASKS_EXIT,
        id::ENGINE_TEXT_SCHTASKS_EXIT_OUTPUT,
        id::ENGINE_TEXT_SCHTASKS_SPAWN,
        id::ENGINE_TEXT_SERVER_URL_INVALID,
        id::ENGINE_TEXT_SERVER_URL_NO_HOST,
        id::ENGINE_TEXT_SERVER_URL_REQUIRED,
        id::ENGINE_TEXT_SERVER_URL_SCHEME,
        id::ENGINE_TEXT_SET_AUTOSTART_FAILED,
        id::ENGINE_TEXT_STOP_CORE_TIMEOUT,
        id::ENGINE_TEXT_TOKEN_INVALID,
        id::ENGINE_TEXT_UPDATE_DOWNLOADING,
        id::ENGINE_TEXT_UPDATE_IN_PROGRESS,
        id::ENGINE_TEXT_VERSION_INVALID,
        id::ENGINE_TEXT_WRITE_CONFIG_FAILED,
        id::ENGINE_LOG_AUTH_FAILURES_LIMIT,
        id::ENGINE_LOG_AUTO_APPLY,
        id::ENGINE_LOG_AUTO_APPLY_FAILED,
        id::ENGINE_LOG_AUTOSTART_DISABLED,
        id::ENGINE_LOG_AUTOSTART_ENABLED,
        id::ENGINE_LOG_CLEAN_TEMP_FILE,
        id::ENGINE_LOG_CLEAN_TEMP_FILE_FAILED,
        id::ENGINE_LOG_CLEAR_DEVICE_FAILED,
        id::ENGINE_LOG_CLIENT_STARTED,
        id::ENGINE_LOG_CONFIG_APPLIED,
        id::ENGINE_LOG_CONFIG_CHANGED_SYNC,
        id::ENGINE_LOG_CONFIG_CHANGED_WITHOUT_BODY,
        id::ENGINE_LOG_CONFIG_UNCHANGED_CORE_STARTED,
        id::ENGINE_LOG_CONNECT_FAILED,
        id::ENGINE_LOG_CONNECTED_SERVER,
        id::ENGINE_LOG_CORE_EXITED,
        id::ENGINE_LOG_CORE_READY,
        id::ENGINE_LOG_CORE_REVIVED,
        id::ENGINE_LOG_CORE_START_FAILED,
        id::ENGINE_LOG_CORE_START_FAILED_ATTEMPT,
        id::ENGINE_LOG_CORE_STARTED_FROM_DISK,
        id::ENGINE_LOG_CREDENTIAL_RECEIVED,
        id::ENGINE_LOG_CREDENTIAL_RECEIVED_RESTART,
        id::ENGINE_LOG_DOWNLOAD_FAILED,
        id::ENGINE_LOG_DOWNLOAD_STARTED,
        id::ENGINE_LOG_DOWNLOADED_MANUAL_INSTALL,
        id::ENGINE_LOG_HANDOFF_LAUNCHER,
        id::ENGINE_LOG_HEARTBEAT_FAILED,
        id::ENGINE_LOG_HEARTBEAT_RECOVERED,
        id::ENGINE_LOG_IDENTITY_INVALID_CLEARED,
        id::ENGINE_LOG_IGNORE_INVALID_VERSION,
        id::ENGINE_LOG_IGNORE_NO_DIGEST,
        id::ENGINE_LOG_IGNORE_NOT_NEWER,
        id::ENGINE_LOG_INSTALL_STARTED,
        id::ENGINE_LOG_LAUNCHER_CHMOD_FAILED,
        id::ENGINE_LOG_LAUNCHER_DIGEST_MISMATCH,
        id::ENGINE_LOG_LAUNCHER_DOWNLOAD,
        id::ENGINE_LOG_LAUNCHER_DOWNLOAD_FAILED,
        id::ENGINE_LOG_LAUNCHER_REPLACE_FAILED,
        id::ENGINE_LOG_LAUNCHER_THREAD_FAILED,
        id::ENGINE_LOG_LAUNCHER_UPDATE_FAILED,
        id::ENGINE_LOG_LAUNCHER_UPDATED,
        id::ENGINE_LOG_LAUNCHER_VERIFY_FAILED,
        id::ENGINE_LOG_LOCAL_CONFIG_DELETED,
        id::ENGINE_LOG_LOCAL_STATE_READ_FAILED,
        id::ENGINE_LOG_READ_AUTOSTART_FAILED,
        id::ENGINE_LOG_RECONNECT_MANUAL,
        id::ENGINE_LOG_REDOWNLOAD,
        id::ENGINE_LOG_REDOWNLOAD_FOR_INSTALL,
        id::ENGINE_LOG_REGISTERED,
        id::ENGINE_LOG_SAVE_CREDENTIAL_FAILED,
        id::ENGINE_LOG_SETTINGS_SAVED,
        id::ENGINE_LOG_SKIP_FAILED_VERSION,
        id::ENGINE_LOG_STOP_CORE_FAILED,
        id::ENGINE_LOG_UPDATE_READY_MANUAL,
        id::ENGINE_LOG_UPDATE_URL_NOT_ABSOLUTE,
    ];

    fn english() -> HashMap<&'static str, &'static str> {
        parse(ENGLISH_LANG).expect("内嵌英文清单必须可解析")
    }

    #[test]
    fn the_two_catalogues_define_the_same_keys() {
        let zh = chinese();
        let en = english();
        assert_eq!(zh.len(), en.len(), "两个清单的条目数必须相同");
        let zh_keys: BTreeSet<&str> = zh.keys().copied().collect();
        let en_keys: BTreeSet<&str> = en.keys().copied().collect();
        assert_eq!(zh_keys, en_keys, "两个清单的 key 集合必须完全相同");
    }

    #[test]
    fn ids_and_the_chinese_table_are_one_to_one() {
        let zh = chinese();
        let listed: BTreeSet<&str> = ALL_IDS.iter().copied().collect();
        assert_eq!(listed.len(), ALL_IDS.len(), "ALL_IDS 里有重复的常量");
        let table: BTreeSet<&str> = zh.keys().copied().collect();
        assert_eq!(
            table, listed,
            "id 常量与中文清单必须一一对应（不许有孤儿条目）"
        );
    }

    #[test]
    fn catalogue_files_are_utf8_lf_without_bom() {
        for (name, text) in [
            ("chinese.lang", CHINESE_LANG),
            ("english.lang", ENGLISH_LANG),
        ] {
            assert!(!text.starts_with('\u{feff}'), "{name} 不能带 BOM");
            assert!(!text.contains('\r'), "{name} 必须用 LF 换行");
            assert!(text.ends_with('\n'), "{name} 应以换行结尾");
        }
    }

    #[test]
    fn get_returns_the_id_itself_when_it_is_missing() {
        assert_eq!(get("no.such.key"), "no.such.key");
        assert_eq!(get(id::CORE_TEXT_TIMEOUT), "超时");
    }

    #[test]
    fn format_substitutes_numbered_placeholders() {
        assert_eq!(
            format(
                id::STATE_ERROR_KEY_LENGTH,
                &[&"/tmp/client.key", &9usize, &32usize]
            ),
            "/tmp/client.key 长度异常：9（应为 32）"
        );
        assert_eq!(
            format(id::CORE_ERROR_TASKKILL_STATUS, &[&"status 1", &"detail"]),
            "taskkill 退出 status 1：detail"
        );
        assert_eq!(
            format(id::REMOTE_ERROR_API, &[&"设备数已达上限", &409u16]),
            "设备数已达上限（HTTP 409）"
        );
    }

    #[test]
    fn render_repeats_a_placeholder_without_extra_arguments() {
        let (text, matched) = render("{0}-{1}-{0}", &[&1, &2]);
        assert_eq!(text, "1-2-1");
        assert!(matched);
    }

    #[test]
    fn missing_arguments_become_empty_instead_of_panicking() {
        let (text, matched) = render("x{0}y{1}z", &[&7]);
        assert_eq!(text, "x7yz");
        assert!(!matched);
        let (text, matched) = render("{0}", &[]);
        assert_eq!(text, "");
        assert!(!matched);
        let (text, matched) = render("plain", &[&7]);
        assert_eq!(text, "plain");
        assert!(!matched);
    }

    #[test]
    fn parse_ignores_comments_and_blank_lines() {
        let table =
            parse("# 注释\n\n  state.error.io =   {0}  读写失败：{1}  \n").expect("必须可解析");
        assert_eq!(table.len(), 1);
        assert_eq!(table["state.error.io"], "{0}  读写失败：{1}");
    }

    #[test]
    fn parse_accepts_a_valid_catalogue() {
        let table = parse("remote.error.decode = 响应解析失败：{0}\n").unwrap();
        assert_eq!(table["remote.error.decode"], "响应解析失败：{0}");
    }

    #[test]
    fn parse_rejects_a_line_without_equals() {
        assert_eq!(
            parse("state.error.io"),
            Err(ParseError::MissingEquals {
                line: 1,
                text: "state.error.io".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_a_key_with_illegal_characters_or_shape() {
        for bad in [
            "state.error.Io = x",
            "state.error.io! = x",
            "state.error.io-extra = x",
            "state.io = x",
            "state.unknown.io = x",
            ".error.io = x",
        ] {
            assert!(
                matches!(parse(bad), Err(ParseError::InvalidKey { .. })),
                "{bad:?} 必须被拒绝"
            );
        }
    }

    #[test]
    fn parse_rejects_an_empty_value() {
        assert_eq!(
            parse("state.error.io =   "),
            Err(ParseError::EmptyValue {
                line: 1,
                key: "state.error.io".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_a_duplicate_key() {
        assert_eq!(
            parse("state.error.io = a\nstate.error.io = b"),
            Err(ParseError::DuplicateKey {
                line: 2,
                key: "state.error.io".to_string(),
            })
        );
    }

    #[test]
    fn parse_rejects_illegal_braces_and_numbering_gaps() {
        for bad in ["{1}", "{0} {2}", "{x}", "a } b", "{0", "{{0}}"] {
            let text = format!("state.error.io = {bad}");
            assert!(
                matches!(parse(&text), Err(ParseError::BadPlaceholder { .. })),
                "{bad:?} 必须被拒绝"
            );
        }
        assert!(parse("state.error.io = {0} {1}").is_ok());
    }
}
