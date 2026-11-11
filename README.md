# MouseBorder

局域网键鼠与剪贴板共享客户端，Mac ↔ Windows。单一原生实现：Rust 负责输入钩子、网络与剪贴板，界面用 Tauri 2 走系统 WebView（macOS WKWebView / Windows WebView2）。不广播设备信息，只接受用户手动输入 IP + 六位当前配对码直连。

旧 Electron 版已从仓库删除，源码备份在 `归档.zip`。**新旧版协议不兼容**（`pair_result` vs `pair-result`、配对后消息是否包 `{mac, body}` 信封），所以两台机器必须都跑这一版。

## 已验证 / 未验证

| 项目 | 状态 | 依据 |
| --- | --- | --- |
| 握手配对、会话密钥、HMAC 封包与篡改拒绝 | 已验证 | `native/tests/clipboard_link.rs` 实机通过 |
| 剪贴板双向同步（含回声抑制） | 已验证 | 同上 |
| 设置持久化、断线重连 | 已验证逻辑 | `cargo test` 单元测试 |
| macOS 打包体积 | 2.7 MiB / ZIP 1.5 MiB | `native/build-mac.sh` |
| Windows 代码可编译 | 由 CI 验证 | GitHub Actions `windows-latest`；本机 `cargo check --target x86_64-pc-windows-msvc` 会在 `tauri-build` 处失败（缺 SDK 的 `llvm-rc`/`rc.exe`） |
| 接收端注入（移动/滚轮/坏 MAC 拒绝/还原） | 已验证（macOS 实机） | `native/tests/input_link.rs` 实机通过 |
| 多屏并集几何、四侧交接、滚轮单位换算 | 已验证逻辑 | `cargo test` 单元测试 |
| 边缘交接手感 | **未验证** | 需要双机实测 |

macOS 的输入捕获不再使用 `rdev::grab`：它在每次按键时调用 HIToolbox 的 `TISGetInputSourceProperty`，而该 API 断言必须在主队列执行，tap 回调跑在后台线程，第一次物理按键就会 `dispatch_assert_queue_fail` → SIGTRAP。现在由 [native/src/tap.rs](native/src/tap.rs) 自建 `CGEventTap`，只读标量字段，并直接取设备相对位移；注入仍走 `rdev::simulate`。

## 功能

- 手动 IPv4/IPv6 + 六位配对码，配对码可刷新。
- TCP/WebSocket 端口 `39876`；配对后每条消息带 HMAC-SHA256，校验失败静默丢弃。
- 全局输入钩子：越过右/左/上/下任一侧边缘把控制权交给对端；被控端光标顶到**朝向对端的那条边**或 `Ctrl+Alt+Esc` 交还。界面顶部徽章实时显示「本机控制 / 正在控制对端 / 对端控制本机」。
- 鼠标移动、按键、滚轮、常用键盘键注入；文本剪贴板 500ms 轮询同步。
- 几何按**所有显示器的并集矩形**计算（副屏在主屏左/上方时原点为负），每 2s 跟随插拔与分辨率变化。Windows 的绝对移动不走 rdev：它按 `GetSystemMetrics(SM_CXVIRTUALSCREEN)` 归一化，而该值受 DPI 虚拟化影响，缩放桌面上会与 `display_info` 的 DEVMODE 并集不一致；现在自己按同一个并集矩形换算成 0..65535 再 `SendInput`（`VIRTUALDESK`），光标回拉也走这条路径。
- 滚轮在线路上统一为 1/120 格（Windows `WHEEL_DELTA`）：macOS 的像素增量按每格约 10px 换算，注入时保留小数余数累加，触控板的细碎增量不会被逐条截断成 0。
- 交接期间本机幽灵光标贴边时自动回拉，避免绝对坐标推导位移在边缘归零（原 tanking 问题）；macOS 逐屏隐藏光标，副屏上不会留下可见的幽灵光标。
- 断线自动重连（1→2→4→…10s 退避）；配对码错误等不可自愈的情况停止并提示。
- 设置写入 `<appData>/settings.json`，重启后保留。

## 开发

前置：Rust stable + `cargo-tauri`；Mac 需 Xcode Command Line Tools，Windows 需 VS C++ Build Tools 与 Windows SDK。

```sh
cd native
cargo tauri dev     # 调试运行
cargo test          # 34 个单元测试
```

### 本机端到端自测（不需要第二台机器）

用一个已知配对码启动一个实例，再让模拟对端连进来：

```sh
cd native
MOUSEBORDER_PIN=246810 open --stderr /tmp/mouseborder.log ../MouseBorder.app
MOUSEBORDER_TEST_PIN=246810 cargo test --test clipboard_link -- --ignored --nocapture
MOUSEBORDER_TEST_PIN=246810 cargo test --test input_link -- --ignored --nocapture
```

`clipboard_link` 验证握手、HMAC 与剪贴板；它会改写系统剪贴板并在结束时还原，若剪贴板当前不是 UTF-8 文本则直接失败退出而不做有损还原。`input_link` 验证接收端注入：配对后发 `control` + 若干 `move`，断言光标真的落到桌面中心偏移处，再发滚轮断言光标不被带着走，坏 MAC 的包必须被丢弃，最后还原光标位置。运行期间请**不要碰鼠标**（约 2s），物理移动会叠加进断言。

两个测试都要求设置里 `edge_enabled` 为 `false`（或先删掉 `<appData>/settings.json` 再关掉开关），否则真实的边缘接管会吞掉你正在用的键鼠。

环境变量覆盖：`MOUSEBORDER_PORT`（监听端口）、`MOUSEBORDER_PEER_PORT`（连接对端端口）、`MOUSEBORDER_PIN`（固定配对码）。前两个用于在同一台机器上跑两个实例。

## 打包

macOS（Apple Silicon）：

```sh
native/build-mac.sh
```

产出 `MouseBorder.app`（2.7 MiB）与 `MouseBorder-Tauri-mac-arm64.zip`（1.5 MiB）在仓库根目录。脚本会同步覆盖根目录产物，避免发出比源码旧的包。

Windows 绿色版必须在 Windows 上构建——`tauri-build` 要把图标编成 Win32 资源，需要 SDK 的 `llvm-rc`/`rc.exe`，Mac 上没有：

```powershell
native\build-windows.ps1
```

产出 `MouseBorder-Windows-x64.exe`，不安装、不写注册表。脚本会检查 WebView2 Runtime 并打印放行 TCP `39876` 的防火墙命令（需管理员执行）。

不想在本地装 VS Build Tools（约 2.5 GB）时，用仓库自带的云端构建：推送到 `main` 且改动涉及 `native/**`，或在 GitHub 仓库页 **Actions → Build Windows → Run workflow** 手动触发，跑完在该次运行的 **Artifacts** 里下载 `MouseBorder-Windows-x64`。工作流见 [.github/workflows/windows.yml](.github/workflows/windows.yml)，用的就是同一条 `cargo build --release --features tauri/custom-protocol`。

## 双机使用

1. 两台机器都启动本版本，接入彼此可路由的网络。Windows 用 `ipconfig` 取网卡 IP。
2. 在 Mac 输入 Windows 的 IP 与 Windows 界面上的六位配对码，点连接。
3. 授予权限（见下），打开边缘切换；两台机器的“屏幕边缘”要选成彼此相邻的那一侧。
4. 复制文本自动同步；`Ctrl+Alt+Esc` 立即回到本机。

## 权限

macOS 需要**辅助功能**（`kTCCServiceAccessibility` + `kTCCServicePostEvent`，用于拦截与注入）和**输入监控**（`kTCCServiceListenEvent`，用于捕获）两项授权，指向 `.app` 而不是内部二进制：从终端直接执行 `MouseBorder.app/Contents/MacOS/…` 时，TCC 会把责任进程算作你的终端，授权对象就错了。用 `open MouseBorder.app` 启动。

事件钩子只在创建 `CGEventTap` 的那一刻读取权限，所以授权后不必重启——界面「控制设置 → 输入捕获」会显示 `运行中 / 未启动`，点「重试捕获」即可。若仍显示 `未启动`，用 `log show --predicate 'process=="tccd"'` 看 `kTCCServiceAccessibility`、`kTCCServicePostEvent`、`kTCCServiceListenEvent` 的 `authValue`（1=允许、0=拒绝、2=受限）。

钩子建立成功或被拒都会写一行 stderr，用 LaunchServices 启动即可捕获（不要直接跑内部二进制，那样 TCC 会把责任进程算作终端）：

```sh
open --stderr /tmp/mouseborder.log MouseBorder.app ; cat /tmp/mouseborder.log
```

当前包已用本地自签身份 **MouseBorder Dev Code Signing** 签名，指定要求固定为

```
identifier "com.mouseborder.native" and certificate leaf = H"c5df012b60244ad959af49bcbf7495a3c523ed08"
```

判定依据是证书指纹而不是每次构建都变的 cdhash，所以 `native/build-mac.sh` 现在每次构建后自动重签，**重建不再需要重新授权**。脚本在找不到该身份时会退回 adhoc 并打印警告（那种情况下权限会再次失效）。换机器时需要重新生成并导入这把身份（钥匙串访问 → 证书助理 → 制作证书，身份类型「自签名根证书」，证书类型「代码签名」），或从本机导出的 `.p12` 备份导入。

`spctl -a` 仍会报 `rejected`：Gatekeeper 不信任自签证书，这只影响从网络下载并带隔离属性的副本；本机自己构建的没有隔离属性，直接运行。分发给别人需要 Apple 开发者证书 + 公证。

Windows 由防火墙控制专用网络入站。

## 安全

不做局域网广播。六位配对码只适合可信局域网临时配对；握手未启用 TLS，会话密钥明文下发，HMAC 只防伪造不防中间人——不要用不可信网络传敏感剪贴板内容。

## 已知边界

多显示器按并集矩形处理，非矩形摆放（例如副屏只占主屏上半段）时并集里的空隙不是可落点，光标会被系统夹到最近的屏幕；键盘布局与系统快捷键未做映射；未做心跳与超时探测；`Ctrl+Alt+Esc` 依赖本机仍能收到键盘事件；macOS 每格滚轮的像素数按 10px 估算，滚动幅度与本机手感可能有偏差；未签名/未公证，分发给别的 Mac 需要在“隐私与安全性”里手动放行。
