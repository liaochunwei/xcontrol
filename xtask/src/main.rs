//! `xtask` — 仓库开发任务驱动（docs/08 §1：构建/打包/基准/审查等）。
//!
//! 用法（workspace 根目录）：
//!
//! ```text
//! cargo run -p xtask -- check-deps   # 依赖方向自检（红线审查，CI 每平台调用）
//! cargo run -p xtask -- help         # 显示帮助
//! ```
//!
//! `check-deps` 是 docs/08 §1 分层红线的机械断言（PRD 要求的 CI 审查步骤）：
//!
//! 1. 直接依赖白名单：`xcontrol-proto` / `xcontrol-core`（红线）与
//!    `xcontrol-config`（I/O 边界 crate，非红线）的直接依赖（normal + dev）
//!    必须全部在登记清单内——新增依赖必须先过 docs 评审再登记；
//! 2. 平台依赖黑名单：红线 crate 全依赖树（含传递依赖）不得出现平台 API / tokio / IO crate
//!    （xcontrol-config 的树经 xcontrol-core 被此扫描间接覆盖；
//!    其 `std-paths` feature 的 `dirs` 默认不进依赖树，故不影响本闸门）。
//!
//! cargo tree 按 host target 解析，本命令在 CI 的三个平台各跑一次，
//! 覆盖 target 门控依赖在不同平台上的解析差异。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 红线 crate：零平台依赖、零 I/O、无 tokio（docs/08 §1 红线、architecture-rules.md 红线 1）。
const RED_LINE_CRATES: [&str; 2] = ["xcontrol-proto", "xcontrol-core"];

/// 白名单审查 crate（非红线）：xcontrol-config 是配置 I/O 边界（允许 std::fs 同步读写，
/// 禁止网络 I/O 与 tokio）；xcontrol-net 是网络边界 crate（网络 I/O 与加密为其职责，
/// M2 起归属 tokio 运行时执行域，docs/01 §2）；xcontrol-input 是平台输入边界 crate
/// （11-17-m2-input-macos 起，CGEvent/平台绑定为其职责——唯一预期 unsafe 的层，docs/08 §2）；
/// xcontrol-clipboard 是平台剪贴板边界 crate（11-17-m2-clipboard-macos 起，arboard 读写与
/// objc2-app-kit NSPasteboard 绑定为其职责，docs/05 §1——与 input 同为预期 unsafe 层；
/// 11-17-m3-clipboard-win-x11 起平台面扩至 windows crate 消息窗口 / x11rb XFixes 监听，
/// mac 平台段零 unsafe、windows 段预期 unsafe、x11 段零 unsafe）；
/// xcontrol-tray 是托盘适配层 crate（11-18-m4-tray 起，gpui-tray 直连为其职责，ADR-0001
/// ——纯 std 投影 + `gpui` feature 门控的 gpui-tray 集成 + 三平台自启，全程 forbid(unsafe_code)）。
/// 其直接依赖同样必须登记过审；全依赖树黑名单扫描仍只针对红线 crate（proto/core 零平台
/// 依赖、零 I/O），此处只做直接依赖白名单断言。
///
/// 注意：input/clipboard/tray 不被 core 依赖（依赖图 app/cli → input/clipboard/tray），故
/// proto/core 的全树黑名单扫描**不覆盖三者的传递树**——core-graphics/objc2-app-kit/
/// arboard/gpui-tray 等平台绑定 crate 只受本白名单直接依赖断言约束（与 net 的 tokio/mio
/// 传递树同待遇，防御纵深缺口见 design.md 记录）。
const GATED_CRATES: [&str; 7] = [
    "xcontrol-config",
    "xcontrol-net",
    "xcontrol-input",
    "xcontrol-clipboard",
    "xcontrol-tray",
    "xcontrol-app",
    "xcontrol-cli",
];

/// 直接依赖的白名单 = CI 审查闸门：新增依赖必须显式过审后才可登记。
/// 依据 docs/08 §1 依赖图登记；新增依赖双登记（architecture-rules.md）：
/// 根 `[workspace.dependencies]` + 本表。
const RED_LINE_ALLOWED_DEPS: &[(&str, &[&str])] = &[
    (
        // 11-16-m1-proto 任务过审（docs/02 §4：帧层 + 全消息编解码）：
        // postcard（CtrlMsg/Clip* 序列化）、serde（postcard derive 前端要求直接依赖）、
        // thiserror（ProtoError）；criterion 仅 dev-dep（benches 编解码吞吐）。
        "xcontrol-proto",
        &[
            "xcontrol-proto",
            "postcard",
            "serde",
            "thiserror",
            "criterion",
        ],
    ),
    (
        // 11-16-m1-core 任务过审（docs/02 §3.3/§4.5）：thiserror（会话错误类型，
        // docs/08 §2 错误规范）、blake3（session_id = BLAKE3(hs_hash)[0..16] 派生，
        // 纯软实现无 I/O，传递树仅 constant_time_eq/cfg-if，已过全树黑名单扫描）。
        "xcontrol-core",
        &[
            "xcontrol-core",
            "xcontrol-proto",
            "xcontrol-config",
            "thiserror",
            "blake3",
        ],
    ),
    (
        // xcontrol-config 首批依赖（11-15-xcontrol-config 任务过审）；
        // dirs 仅其 `std-paths` feature 使用（可选依赖，默认不进依赖树）。
        // 11-19-m6-security 增补：keyring 仅其 `keychain` feature 使用（可选
        // 依赖，OS 钥匙串真接线 docs/07 §3.3；三平台后端 target 内部门控，
        // linux 走 secret-service+zbus 纯 Rust——查树记录见根
        // workspace.dependencies keyring 条目）。默认编译面零增量，
        // proto/core 红线树不受影响（check-deps 全树扫描默认 feature 面）。
        "xcontrol-config",
        &[
            "xcontrol-config",
            "serde",
            "toml",
            "thiserror",
            "dirs",
            "keyring",
        ],
    ),
    (
        // 11-16-m1-net 任务过审（docs/02 §3、ADR-0002）：snow（Noise XXpsk0 握手与
        // 传输态，内建 nonce/rekey，禁止自造 AEAD 层）、argon2（PSK 派生 KDF，
        // std 最小 feature）、blake3（salt/psk 域分离派生）、thiserror（NetError）、
        // criterion 仅 dev-dep（benches/loopback.rs 回环延迟基准）；
        // xcontrol-core 仅 dev-dep（tests/chain.rs 全链路集成测试专用——集成测试
        // 消费 core 的 Session/Dispatcher/InputSink；生产依赖方向仍为 net → proto + config，
        // docs/08 §1 依赖图）。
        // 11-17-m2-tcp 增补：tokio（真实 TCP 传输运行时，docs/02 §1、ADR-0002 决策 1；
        // default-features off + 最小 feature 集 rt-multi-thread/net/io-util/macros/
        // time/sync——详见根 workspace.dependencies 注释）。传递树 mio/windows-sys 属
        // net 合法平台依赖（全树黑名单扫描只针对红线 crate proto/core；net 为网络
        // 边界 crate，网络 I/O 与运行时是其职责，docs/01 §2）。
        // 11-17-m3-discovery 增补：mdns-sd（mDNS 通告/浏览，docs/02 §2.1，ADR-0002
        // 决策 3——自带后台守护线程 + flume channel；传递树 fastrand/flume/
        // if-addrs/mio/socket2/socket-pktinfo 纯软或 socket bindings）、if-addrs
        // （网络接口枚举，docs/02 §2.1 beacon 逐接口定向广播；unix libc / windows
        // windows-sys 传递树同上待遇）、socket2（发现 UDP 口复用绑定的预绑定选项）。
        "xcontrol-net",
        &[
            "xcontrol-net",
            "xcontrol-proto",
            "xcontrol-config",
            "snow",
            "argon2",
            "blake3",
            "thiserror",
            "criterion",
            "xcontrol-core",
            "tokio",
            "mdns-sd",
            "if-addrs",
            "socket2",
            // 11-21 大图断连排查过审：net::transport 层直接观测（流错位重同步
            // 锁定候选/解密失败——net 层静默是本次排查盲区根因之一，见
            // logging-targets.md `net::transport` 登记）。
            "tracing",
        ],
    ),
    (
        // 11-17-m2-input-macos 任务过审（docs/05 §1、ADR-0003）：xcontrol-core
        // （re-export core::sink 的 InputSource/InputSink trait——docs/08 §1 依赖图已随本任务
        // 补录 input → core 方向）、thiserror（InputError）、core-graphics/core-foundation
        // （macOS CGEventTap 捕获 + CGEventPost 注入 + AXIsProcessTrusted 权限检测，
        // 仅 macos feature + target 双门控；API 签名逐项对 docs.rs 0.25/0.10 核实）。
        // 平台绑定 crate 出现在黑名单（FORBIDDEN_PACKAGES）对 proto/core 生效；
        // input 自身不受全树扫描覆盖（见 GATED_CRATES 注释），直接依赖必须全部过审。
        // 11-17-m3-input-win 增补：windows（微软官方 Win32 bindings——WH_*_LL hook 捕获 +
        // SendInput 注入 + GetCursorPos/GetSystemMetrics 查询，docs/05 §2；仅 windows
        // feature + target 双门控，default-features off，签名逐项对 docs.rs 0.62 源码核实；
        // 传递树仅 windows-core/windows-strings 微软自系 bindings）。
        // 11-17-m3-input-x11 增补：x11rb（纯 Rust X11 bindings——XRecord 被动捕获 +
        // XTEST 注入 + grab/warp/query_pointer 光标语义，docs/05 §3；仅 x11 feature + target
        // 双门控，default-features off = 不拉 libc/libxcb（未开 allow-unsafe-code，crate 内
        // forbid(unsafe_code)；传递树仅 x11rb-protocol + gethostname/rustix 纯 Rust socket
        // I/O）；签名逐项对 registry 0.13.2 源码核实，且与 gpui 传递树中的 x11rb 0.13.2
        // 同版同源）。
        // 11-19-m5-wayland 增补（docs/05 §4、ADR-0003）：ashpd（XDG portals——InputCapture
        // 捕获会话 + RemoteDesktop 注入会话 + GlobalShortcuts 热键 + Clipboard 探测；仅
        // wayland feature + target 双门控；0.13.13 与 gpui 传递树同版同源，零新增版本；
        // 签名逐项对 registry 0.13.13 源码核实）、reis（纯 Rust libei——EIS fd 同步事件
        // 协议，default-features off = 事件路径零 async；签名对 crates.io 0.7.1 解包对源）、
        // evdev（uinput 虚拟设备兜底注入，docs/05 §4.3；传递树 libc/nix 0.29/bitvec 为
        // 新增包——系统调用 bindings 与纯软实现，不进 proto/core 红线树）、tokio（portal
        // DBus 调用执行域——模块内自建 current-thread runtime 控制线程专用；input 为
        // GATED 平台边界 crate，tokio 待遇同 net 先例）、futures-util（ashpd 信号流
        // StreamExt——0.3.34 已在 lock，零新增版本）、tracing（wayland 模块
        // input::wayland 子系统日志——纯门面，docs/06 §6 子系统命名先例）。
        // 11-22 边缘回切修复轮增补：wayland-client（wl_output 枚举真实分辨率——uinput
        // ABS 域标定替换 (1920,1080) 硬编码；0.31 已在 lock 经 wl-clipboard-rs/gpui 传递
        // 同版合并，零新增版本/包；仅 wayland feature + target=linux 双门控；一次性阻塞
        // roundtrip 查询，非事件热路径。FORBIDDEN_PACKAGES 内的 wayland-client 系只对
        // proto/core 红线树生效，input 是 GATED 平台边界 crate——直接依赖经本白名单
        // 过审即合法，登记依据见根 workspace.dependencies 注释）。
        "xcontrol-input",
        &[
            "xcontrol-input",
            "xcontrol-proto",
            "xcontrol-config",
            "xcontrol-core",
            "thiserror",
            "core-graphics",
            "core-foundation",
            "windows",
            "x11rb",
            "ashpd",
            "reis",
            "evdev",
            "tokio",
            "futures-util",
            "tracing",
            "async-io",
            "wayland-client",
        ],
    ),
    (
        // 11-17-m2-clipboard-macos 任务过审（docs/05 §1、docs/04 §4/§5）：thiserror
        // （ClipboardError）、blake3（写入指纹 = blake3 短哈希，docs/04 §5 防线 1，
        // 纯软实现无 I/O）、arboard（剪贴板读写高层封装，docs/05 §0"剪贴板读写"行；
        // 仅经平台 feature 的 dep:arboard 启用）、objc2-app-kit（NSPasteboard
        // .changeCount 原生轮询——mac 无剪贴板变化通知 API，docs/04 §4；arboard
        // 不暴露 changeCount。仅 macos feature + target 双门控，最小 feature
        // std/NSPasteboard）。xcontrol-core 仅 dev-dep（测试用 core 的 MockClipboard
        // 对拍 ClipboardWatch trait 语义——M1 mock 接口形状，docs/08 §1 依赖图无
        // 新增生产方向；先例：net 的 dev-dep xcontrol-core 同待遇）。平台绑定 crate
        // （arboard/objc2 系）出现在黑名单（FORBIDDEN_PACKAGES）对 proto/core 生效；
        // clipboard 不被 core 依赖，不受全树扫描覆盖（见 GATED_CRATES 注释）。
        // 11-17-m3-clipboard-win-x11 增补：windows（AddClipboardFormatListener 消息
        // 窗口监听 + GetClipboardSequenceNumber 计数原语，docs/05 §2 剪贴板行；仅
        // windows feature + target 双门控，default-features off，签名逐项对 0.62.2
        // 源码核实——与 xcontrol-input 共享同一 workspace 登记，无第二版本）；
        // x11rb（XFixesSelectSelectionInput → SelectionNotify 监听，docs/05 §3 剪贴板
        // 行；仅 x11 feature + target 双门控——与 xcontrol-input 共享同一 workspace
        // 登记，0.13 与 gpui 传递树同版；arboard 的 x11rb 传递依赖同版同源，无第二
        // 版本，allow-unsafe-code 未开保持零 unsafe）。
        // 11-19-m5-clipboard-full 增补（FR-4.2/4.3 + Wayland，docs/04 §2、docs/05 §4）：
        // - png：PNG 编解码（png_codec 公共模块——RGBA↔wire PNG 互转 + 图片回声指纹
        //   规范化解码，非平台门控；0.18.1 已在 gpui→image 传递树，零新增包）；
        // - objc2 / objc2-foundation：mac public.html 挂载（autoreleasepool 纪律 +
        //   NSString 载体；与 objc2-app-kit 0.3.2 传递依赖同版，仅 macos feature +
        //   target 双门控）；
        // - wl-clipboard-rs：Wayland 剪贴板读写 + watch::Watcher 监听（docs/05 §4；
        //   0.9.5 首含 watch 模块；仅 wayland feature + target=linux 双门控；wayland
        //   传递树 wayland-client 系与 gpui 同版合并）。
        "xcontrol-clipboard",
        &[
            "xcontrol-clipboard",
            "xcontrol-proto",
            "xcontrol-config",
            "xcontrol-core",
            "thiserror",
            "blake3",
            "arboard",
            "objc2-app-kit",
            "objc2",
            "objc2-foundation",
            "windows",
            "x11rb",
            "png",
            "wl-clipboard-rs",
        ],
    ),
    (
        // 11-18-m4-tray 任务过审（docs/06 §5、ADR-0001）：托盘适配层从 M0 空壳转正。
        // - gpui-tray（托盘直连，docs/06 §1 定稿）+ gpui-kit（gpui 唯一入口线，ADR-0001）
        //   ——两者均为 `gpui` feature 的 optional 依赖，仅 xcontrol-app 开启；check-deps
        //   按 default features 解析（gpui off），故二者在白名单中是"登记在案、按需生效"
        //   ——真实进入依赖树发生在 app 的构建面（app 白名单已含 xcontrol-tray，gpui-tray
        //   属其传递树不在此直接断言，双重保障 = 本登记 + cargo-deny license 审查）。
        // - thiserror：AutostartError（error-handling.md 规则 1——库 crate thiserror）。
        // - windows-registry：HKCU Run 键自启（docs/05 §2），target=windows 门控——非
        //   windows host 的 cargo tree 不含它（三平台各跑一次的 check-deps 覆盖）。
        //   注意：windows-registry 在 FORBIDDEN_PACKAGES 黑名单内是针对 proto/core 红线树
        //   的防御；tray 为 GATED crate（平台边界层），直接依赖经本白名单过审即合法。
        // - 平台自启实现（launchd/XDG）为纯 std（fs + Command），无第三方依赖。
        "xcontrol-tray",
        &[
            "xcontrol-tray",
            "xcontrol-i18n",
            "gpui-tray",
            "gpui-kit",
            "thiserror",
            "windows-registry",
        ],
    ),
    (
        // 11-17-m2-app 任务过审（docs/06 §1/§6、ADR-0001）：gpui-kit（gpui 唯一入口，
        // M2 起 default-features off = spike 已验证组合；11-18-m4-ui 起开启组件库
        // default features——gpui-component 0.9.1/assets 经其传递树进入，不另立直接
        // 依赖条目，版本源单一断言见 tests::gpui_version_source_is_single）、
        // tracing + tracing-subscriber（fmt 层，docs/06 §6 日志
        // 子系统 app::/session::/net::）、anyhow（bin 顶层错误处理，error-handling.md 规则
        // 2——app 是进程入口， anyhow 允许且仅允许出现在 bin）、tokio（session_rt 执行域，
        // docs/01 §2；UI 主线程归 gpui，经 state.rs watch/mpsc 单向桥接）。内部 crate 全部
        // 过审登记；本 crate 是依赖汇聚点（依赖图顶点），传递树不参与红线黑名单扫描
        // （app 不被任何 crate 依赖；proto/core 全树扫描不受影响）。
        // 11-17-m3-status 增补：tracing-appender（NFR-8 滚动文件层，daily rotation 写
        // data_dir/logs/、前缀 xcontrol.log；传递树 crossbeam-channel/time 纯软实现）；
        // tracing-subscriber 增开 env-filter feature（级别化过滤，config [general].log_level
        // + --log-level + XCONTROL_LOG；传递树 matchers→regex-automata/regex-syntax 纯
        // Rust——feature 变更非新依赖，不另开条目）。
        "xcontrol-app",
        &[
            "xcontrol-app",
            "xcontrol-proto",
            "xcontrol-config",
            "xcontrol-core",
            "xcontrol-net",
            "xcontrol-input",
            "xcontrol-clipboard",
            "xcontrol-tray",
            "xcontrol-i18n",
            "gpui-kit",
            "tracing",
            "tracing-subscriber",
            "tracing-appender",
            "anyhow",
            "tokio",
        ],
    ),
    (
        // 11-17-m3-status 过审（NFR-8 --status）：M2 起空壳时依赖就与 app 同形（内部
        // crate 全集，依赖图顶点待遇同 app——传递树不参与红线黑名单扫描）；本任务起
        // 首批真实功能落地并纳入 GATED_CRATES 直接依赖白名单断言：xcontrol-config 开
        // std-paths（default_config_path/default_data_dir，读配置摘要与日志目录）、
        // anyhow（bin 顶层错误处理，error-handling.md 规则 2）、serde_json（--status
        // --json 机器可读输出，design.md 定夺——手写 JSON 转义负担不值得省）。
        "xcontrol-cli",
        &[
            "xcontrol-cli",
            "xcontrol-proto",
            "xcontrol-config",
            "xcontrol-core",
            "xcontrol-net",
            "xcontrol-input",
            "xcontrol-clipboard",
            "xcontrol-tray",
            "anyhow",
            "serde_json",
        ],
    ),
];

/// 已知平台 API / 运行时 / IO crate 黑名单（对红线 crate 全依赖树扫描，防御纵深）。
const FORBIDDEN_PACKAGES: &[&str] = &[
    // 本仓库平台 crate（docs/08 §1 依赖图：proto/core 只允许依赖彼此与 config）
    "xcontrol-input",
    "xcontrol-clipboard",
    "xcontrol-tray",
    "xcontrol-net",
    "xcontrol-app",
    "xcontrol-cli",
    // 平台 API 绑定（docs/01 §6、docs/05）
    "core-graphics",
    "core-graphics-types",
    "core-foundation",
    "core-foundation-sys",
    "core-audio-types",
    "metal",
    "objc2",
    "objc2-app-kit",
    "objc2-foundation",
    "cocoa",
    "security-framework",
    "system-configuration",
    "windows",
    "windows-sys",
    "windows-core",
    "windows-registry",
    "windows-strings",
    "x11rb",
    "x11-dl",
    "xkbcommon",
    "wayland-client",
    "wayland-backend",
    "wayland-sys",
    "wayland-protocols",
    "wayland-protocols-wlr",
    "wayland-scanner",
    "ashpd",
    "reis",
    "wl-clipboard-rs",
    "tray-icon",
    "ksni",
    "arboard",
    "enigo",
    "rfd",
    "gio",
    "glib",
    "gtk",
    "gtk4",
    // 运行时 / IO（红线 1：proto/core 禁止 tokio 运行时依赖）
    "tokio",
    "tokio-util",
    "tokio-stream",
    "mio",
    "smol",
    "async-std",
    "libc",
    "nix",
];

fn main() {
    let exit_code = match std::env::args().nth(1).as_deref() {
        None | Some("help") | Some("-h") | Some("--help") => {
            print_usage();
            0
        }
        Some("check-deps") => match check_deps() {
            Ok(()) => 0,
            Err(message) => {
                eprintln!("check-deps FAILED:\n{message}");
                1
            }
        },
        Some(unknown) => {
            eprintln!("未知子命令：{unknown}");
            println!();
            print_usage();
            2
        }
    };
    if exit_code != 0 {
        std::process::exit(exit_code);
    }
}

fn print_usage() {
    println!("xtask — XControl 开发任务驱动（docs/08 §1）");
    println!();
    println!("用法：cargo run -p xtask -- <子命令>");
    println!();
    println!("子命令：");
    println!(
        "  check-deps   依赖方向自检：断言 xcontrol-proto / xcontrol-core 零平台依赖 + 全部白名单 crate 依赖过审（CI 审查步骤）"
    );
    println!("  help         显示本帮助");
}

/// 执行依赖方向自检；违规时返回逐条违规说明（调用方以非零码退出）。
fn check_deps() -> Result<(), String> {
    let cargo = cargo_binary();
    let root = workspace_root();
    let mut violations: Vec<String> = Vec::new();

    for pkg in RED_LINE_CRATES {
        check_direct_deps_whitelist(&cargo, &root, pkg, &mut violations)?;
        // 红线 crate 另做全依赖树黑名单扫描（防御纵深，含传递依赖）。
        // **只扫 normal 边**（11-25 裁定，PR #1 windows job 实测）：黑名单 =
        // 零平台依赖 / 零 I/O 的运行时红线；dev 依赖（criterion 基准工具等）
        // 不进运行时，且 criterion 在 windows 上传递 windows-sys（walkdir 系）
        // ——把 dev 树扫进黑名单会把"基准工具在其目标平台上的正常依赖"误报
        // 成红线违规。dev 依赖本身由上方直接依赖白名单同等审住（criterion
        // 已过审登记），运行时红线不被稀释。
        let tree = cargo_tree_names(&cargo, &root, pkg, &["--edges", "normal"])?;
        for name in &tree {
            if FORBIDDEN_PACKAGES.contains(&name.as_str()) {
                violations.push(format!(
                    "{pkg}: 依赖树中出现禁用的平台/IO crate `{name}`（红线：零平台依赖、零 I/O）"
                ));
            }
        }
    }

    for pkg in GATED_CRATES {
        check_direct_deps_whitelist(&cargo, &root, pkg, &mut violations)?;
    }

    if violations.is_empty() {
        println!(
            "check-deps: PASS（红线 crate 零平台依赖、零 I/O；白名单 crate 依赖过审，docs/08 §1）"
        );
        Ok(())
    } else {
        Err(violations.join("\n"))
    }
}

/// 直接依赖白名单断言：`cargo tree --depth 1` 的结果必须全部登记在
/// `RED_LINE_ALLOWED_DEPS`；违规追加到 `violations` 并打印单 crate 结论。
fn check_direct_deps_whitelist(
    cargo: &Path,
    root: &Path,
    pkg: &str,
    violations: &mut Vec<String>,
) -> Result<(), String> {
    let marked_at = violations.len();
    let allowed = allowed_deps_for(pkg)?;

    let direct = cargo_tree_names(cargo, root, pkg, &["--edges", "normal,dev", "--depth", "1"])?;
    let direct_deps: Vec<&str> = direct
        .iter()
        .filter(|name| name.as_str() != pkg)
        .map(String::as_str)
        .collect();
    for dep in &direct_deps {
        if !allowed.contains(dep) {
            violations.push(format!(
                "{pkg}: 直接依赖 `{dep}` 未登记（docs/08 §1 依赖图）——先改 docs 评审，再更新 xtask 白名单"
            ));
        }
    }

    let verdict = if violations.len() == marked_at {
        "OK"
    } else {
        "VIOLATED"
    };
    println!(
        "check-deps: {pkg} {verdict}（直接依赖 {} 个：{}）",
        direct_deps.len(),
        direct_deps.join(", ")
    );
    Ok(())
}

fn allowed_deps_for(pkg: &str) -> Result<&'static [&'static str], String> {
    RED_LINE_ALLOWED_DEPS
        .iter()
        .find(|(crate_name, _)| *crate_name == pkg)
        .map(|(_, allowed)| *allowed)
        .ok_or_else(|| format!("内部错误：{pkg} 未登记依赖白名单"))
}

/// 运行 `cargo tree -p <pkg> --prefix none` 并收集包名集合。
fn cargo_tree_names(
    cargo: &Path,
    root: &Path,
    pkg: &str,
    extra_args: &[&str],
) -> Result<BTreeSet<String>, String> {
    let output = Command::new(cargo)
        .arg("tree")
        .arg("-p")
        .arg(pkg)
        .args(extra_args)
        .args(["--prefix", "none"])
        .current_dir(root)
        .output()
        .map_err(|error| format!("无法执行 cargo tree（{cargo:?}）：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree -p {pkg} 失败（exit {}）：{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("cargo tree 输出不是 UTF-8：{error}"))?;
    Ok(stdout.lines().map(package_name).collect())
}

/// 解析 `cargo tree --prefix none` 的行：`name vX.Y.Z (source)`；
/// 重命名依赖显示为 `name vA (rename vX.Y.Z)`，取首个空格前字段即包名。
/// 防御性剥离树形符号（万一 prefix 参数被后续维护者改动）。
fn package_name(line: &str) -> String {
    line.trim_start_matches([' ', '│', '├', '└', '─'])
        .split(' ')
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// `cargo run` 环境下 `CARGO` 指向当前 cargo；直接执行时回退 PATH。
fn cargo_binary() -> PathBuf {
    std::env::var_os("CARGO").map_or_else(|| PathBuf::from("cargo"), PathBuf::from)
}

/// xtask 固定位于仓库根 `xtask/`，工作区根即其父目录。
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::package_name;

    /// 占位单测：保证 CI `cargo test --workspace` 对 xtask 非空跑（M0 任务验收项）。
    #[test]
    fn crate_skeleton_smoke() {
        assert_eq!(env!("CARGO_PKG_NAME"), "xtask");
    }

    #[test]
    fn package_name_parses_cargo_tree_line() {
        assert_eq!(
            package_name("xcontrol-proto v0.1.0 (/repo/crates/xcontrol-proto)"),
            "xcontrol-proto"
        );
    }

    #[test]
    fn package_name_ignores_tree_glyphs() {
        assert_eq!(
            package_name("├── xcontrol-config v0.1.0 (/repo/crates/xcontrol-config)"),
            "xcontrol-config"
        );
    }

    /// 解析 workspace `Cargo.lock` 的 `[[package]]` 块 → (name, version) 对
    /// （纯 std 行解析：lock v3+ 的依赖列表是字符串数组，无裸 name/version 行）。
    fn lockfile_packages() -> Vec<(String, String)> {
        let lock_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock");
        let raw = std::fs::read_to_string(&lock_path).expect("workspace Cargo.lock 可读");
        let mut packages = Vec::new();
        let mut current_name: Option<String> = None;
        for line in raw.lines() {
            let line = line.trim();
            if line == "[[package]]" {
                current_name = None;
                continue;
            }
            if let Some(rest) = line.strip_prefix("name = \"") {
                current_name = Some(rest.trim_end_matches('"').to_owned());
                continue;
            }
            if let Some(rest) = line.strip_prefix("version = \"") {
                let version = rest.trim_end_matches('"').to_owned();
                if let Some(name) = current_name.take() {
                    packages.push((name, version));
                }
            }
        }
        packages
    }

    /// gpui 版本源单一断言（ADR-0001 决策 1 红线，11-18-m4-ui check 补自动化
    /// ——此前仅有 cargo tree 人工核对）：gpui-pre 快照家族必须同一版本
    /// （当前 0.3.12），gpui-kit / gpui-component / gpui-tray 各只允许一个
    /// 版本源——第二版本源 = gpui 类型不共享 = 编译期冲突，红线在此收口。
    /// 注：`gpui-pre-reqwest` 是独立版本线的伴生 crate（随上游 reqwest 0.12
    /// 版本号），不属于快照家族，显式排除。
    #[test]
    fn gpui_version_source_is_single() {
        let packages = lockfile_packages();

        let pre_versions: std::collections::BTreeSet<&str> = packages
            .iter()
            .filter(|(name, _)| {
                (name == "gpui-pre" || name.starts_with("gpui-pre-")) && name != "gpui-pre-reqwest"
            })
            .map(|(_, version)| version.as_str())
            .collect();
        assert_eq!(
            pre_versions.iter().copied().collect::<Vec<_>>(),
            vec!["0.3.12"],
            "gpui-pre 全家族必须单一 0.3.12 版本源（ADR-0001）：{pre_versions:?}"
        );

        for pinned in ["gpui-kit", "gpui-component", "gpui-tray"] {
            let versions: Vec<&str> = packages
                .iter()
                .filter(|(name, _)| name == pinned)
                .map(|(_, version)| version.as_str())
                .collect();
            assert_eq!(
                versions.len(),
                1,
                "{pinned} 必须单一版本源（ADR-0001）：{versions:?}"
            );
        }
        assert!(
            packages.iter().any(|(name, _)| name == "gpui-component"),
            "组件库（gpui-kit default features）应已进入依赖树（11-18-m4-ui 组件裁定）"
        );
    }
}
