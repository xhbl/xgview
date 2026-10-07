# XGView 电源管理方案（阻止睡眠 / 屏幕常亮 / Android 前台保活）

> 状态：方案归档，待实施。
> 目标：程序运行期间阻止系统睡眠与显示器关闭；Android 上的前台保活列为三期。
> 决策：`prevent_sleep` 与 `keep_screen_on` 两个独立开关，**默认均开启**（监控墙场景）。

## 修订记录

本版按**当前磁盘上的代码**逐条核对初稿，修正了若干处会让实现编不过或不生效的写法：

| # | 初稿 | 问题 | 本版 |
|---|---|---|---|
| 1 | Linux 用 `systemd-inhibit … cat` | `cat` 遇 stdin EOF 立即退出，抑制锁随之释放，等于没加 | 改为 `sleep infinity`，子进程 stdin 设 `null` |
| 2 | Android 在 `static` 方法里调 `getWindow()` | 编不过：`getWindow()` 是实例方法 | 改用**已有的** `static instance` + `runOnUiThread`；`setReserveNavigationBar` 就是现成模板 |
| 3 | 复用 `android.rs` 的 `call_void` / `call_bool` | 两者签名写死 `"()V"` / `"()Z"`，不支持参数 | Java 侧拆成两个单参方法，Rust 加 `call_void_bool` |
| 4 | "复用现有 `default_true()`" | 该函数不存在；而且不需要 | 只在 `impl Default` 里给两个 `true`（`AppConfig` 已有结构级 `#[serde(default)]`） |
| 5 | i18n 键用 `streams-prevent-sleep` 等 | 前缀属于 Streams 页；System 页用 `settings-*` | 改为 `settings-*`，并复用已存在的 `settings-mechanism` |
| 6 | 把"前台保活"列为本期能力 | 本期只有 `FLAG_KEEP_SCREEN_ON` 与 `PARTIAL_WAKE_LOCK`，**都不阻止进程回收** | 移出本期，单列三期 |
| 7 | 二期第 8 步"放开一期的 UI gate" | 一期并没有单独的 gate——置灰来自 `is_supported()` 为假 | 取消 gate 这一概念：`is_supported()` 在 Android 上就等于"钩子装没装上"（§4.2） |

此外补充了初稿没有的几块：两个开关组合的语义矛盾（§2）、可测性切分（§4.1）、Android 钩子为什么必须存在（§4.2）、以及残留与泄漏（§5 / §7 / §8）。

## 1. 背景与现状

监控墙为 7×24 常驻显示场景，设备不应因系统空闲而休眠或关闭显示器，否则画面中断、重连抖动。当前项目**没有任何电源管理代码**：

| 搜索项 | 结果 |
|---|---|
| `SetThreadExecutionState` | 零匹配 |
| `WakeLock` / `PARTIAL_WAKE_LOCK` | 零匹配 |
| `FLAG_KEEP_SCREEN_ON` | 零匹配 |
| `startForeground` / `<service>` | 零匹配 |

**现成入口**：`crates/monitor_android/android/AndroidManifest.xml:47` 已声明 `android.permission.WAKE_LOCK`，其注释本身写的就是 "Keep the screen on while the surveillance wall is displayed"——这个能力当初就是预留的，只是没有任何代码去获取它。

**可参考的既有跨平台模式**：`crates/monitor_core/src/autostart.rs`（257 行）——顶层平台无关 API + 各平台 `imp` 模块 + 错误经 `CoreError` 上报。注意它的 `imp` 只有 windows / `all(unix, not(target_os = "android"))` / android / 兜底四块：**Linux 与 macOS 合在一个 `unix` 模块里**，因为两者机制相同（都是写文件）。电源管理这边两者机制完全不同（`caffeinate` vs `systemd-inhibit`），因此本方案**有意拆成两个 `imp`**——这是对模板的偏离，不是疏忽。

**Android 侧已有可复用的结构**：`MainActivity` 已持有 `private static MainActivity instance`（`onCreate` 赋值、`onDestroy` 置空），且所有 native 入口都是 `instance` + `runOnUiThread` 的形状，实现不需要新建静态引用。

## 2. 目标与需求

三个能力，其中只有前两个在本期交付：

| 能力 | 含义 | 平台范围 | 本期交付 |
|---|---|---|---|
| **阻止系统睡眠** | 系统不进待机 | 全平台 | 是 |
| **保持屏幕常亮** | 显示器不自动关闭、不触发屏保 | 全平台 | 是 |
| **前台保活** | 全屏显示期间防止系统回收进程 | Android | **否**，见 §2.2 |

配置为两个独立开关：

- `prevent_sleep`：阻止系统进入睡眠。
- `keep_screen_on`：保持屏幕常亮。
- 两者默认均 **开启**。

### 2.1 "屏幕常亮"不蕴含"系统不睡"

这一点必须在实现里处理掉，否则两个开关能组合出自相矛盾的状态。

三个平台的"屏幕常亮"机制都**只作用于显示**，都不阻止系统整体进入睡眠：

- Windows：`ES_DISPLAY_REQUIRED` 只重置显示空闲计时器；
- macOS：`caffeinate -d` 只针对显示；
- Linux：`systemd-inhibit --what=idle` 只挡 idle（屏保/熄灭）。

于是"勾了常亮、取消阻止睡眠"会得到一个怪状态：屏幕亮着，但系统仍可能整体睡眠——而系统一睡屏幕自然也灭了，等于这个组合没有意义。

**处理方式：在 `apply()` 里归一化——`keep_screen_on` 为真时，同时请求系统不睡。** UI 文案按此说明，不要把它描述成两个正交的开关。

### 2.2 "前台保活"不在本期

本期的 Android 实现只有 `FLAG_KEEP_SCREEN_ON` 与 `PARTIAL_WAKE_LOCK`。前者是 Activity 级窗口标志、随窗口销毁自动清除；后者只让 CPU 不进入低功耗——**两者都不能阻止系统在内存紧张或后台限制下回收进程**。

真正的保活需要前台服务：manifest 里的 `<service>` + `FOREGROUND_SERVICE` 权限 + 一个常驻通知，Android 14+ 还需要 `foregroundServiceType`（并因此需要对应的 `FOREGROUND_SERVICE_*` 权限）。这与本期两块的成本不在一个量级，**单列为三期**；在它落地之前，UI 不应宣称"保活"。

## 3. 平台 API 矩阵

| 能力 | Windows | macOS | Linux | Android |
|---|---|---|---|---|
| 阻止睡眠 | `SetThreadExecutionState(ES_CONTINUOUS \| ES_SYSTEM_REQUIRED)` | `caffeinate -i` | `systemd-inhibit --what=sleep` | `PARTIAL_WAKE_LOCK` |
| 屏幕常亮 | 同上 `+ ES_DISPLAY_REQUIRED` | 再加 `-d` | 再加 `:idle` | `FLAG_KEEP_SCREEN_ON` |
| 前台保活 | 不适用 | 不适用 | 不适用 | 前台服务（三期） |

释放方式四者各不相同，这是本实现最容易出错的地方：

| 平台 | 释放 | 注意 |
|---|---|---|
| Windows | `SetThreadExecutionState(ES_CONTINUOUS)` | **per-thread** 状态，必须在长寿线程上设置与清除 |
| macOS | kill 子进程 | 加 `-w <pid>` 让它随主进程退出，避免孤儿 |
| Linux | kill 子进程 | `systemd-inhibit` 没有 `-w` 等价物，崩溃会留下残留锁 |
| Android | 清 flag / `release()` WakeLock | WakeLock 必须在 Java 的 `onDestroy` 里也释放一次 |

## 4. 架构设计

新建 `crates/monitor_core/src/power.rs`，模仿 `autostart.rs` 的结构。

```
power.rs
├── struct Applied { system: bool, display: bool }        // 已施加的状态
├── fn plan(applied: Applied, prevent_sleep: bool, keep_screen: bool) -> Option<Applied>
├── pub struct PowerStatus { supported, applied, mechanism, error: Option<String> }
├── pub fn is_supported() -> bool
├── pub fn mechanism() -> &'static str
├── pub fn apply(prevent_sleep: bool, keep_screen: bool) -> Result<()>   // 幂等、重入安全
├── pub fn release() -> Result<()>
├── pub fn status() -> PowerStatus
├── #[cfg(target_os = "android")] pub type AndroidRequest = fn(bool, bool) -> Result<()>
├── #[cfg(target_os = "android")] pub fn install_android(request: AndroidRequest)
├── #[cfg(windows)]                    mod imp — SetThreadExecutionState FFI
├── #[cfg(target_os = "macos")]        mod imp — caffeinate 子进程
├── #[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
│                                      mod imp — systemd-inhibit 子进程
├── #[cfg(all(unix, not(target_os = "android")))] mod holder — 启子进程，并确认它没有立刻退出
├── #[cfg(target_os = "android")]      mod imp — 转发给 GUI 装上的 AndroidRequest
└── #[cfg(not(any(windows, unix)))]    mod imp — CoreError::unsupported
```

> 注意 `not()` 只接受一个谓词，`#[cfg(unix, not(macos, android))]` 是无效语法；上面按 `autostart.rs` 的写法展开。

**内部状态**：`static Mutex<InnerState>` 保存子进程 handle / WakeLock 状态 / 当前 `Applied`。`apply` 幂等——重复调用相同参数不重复施加，参数变化时只做增量调整。

**错误上报**：复用 `CoreError::unsupported` / `CoreError::config`，与 `autostart.rs` 一致。失败要能被 UI 看见（见 §6），不能静默当作成功。

### 4.1 可测的那一半

`imp` 直接调 OS API，逻辑全包在里面就没法测——而单测又不能真的去改系统电源状态。按仓库里 `StallDeadline` / `ReconnectPolicy` 的先例（纯状态 + 断言），把"该做什么"抽成纯函数：

```rust
/// 归一化后的目标与当前已施加状态的差量；`None` 表示无需动作。
fn plan(applied: Applied, prevent_sleep: bool, keep_screen: bool) -> Option<Applied>;
```

`plan` 只做两件事：归一化（`keep_screen ⇒ system`，见 §2.1）与幂等判断（与 `applied` 相同则返回 `None`）。**单测覆盖它**；`apply()` 只负责按 `plan` 的结果去调 `imp`。

### 4.2 为什么 Android 是一个钩子

Android 的两半——`PARTIAL_WAKE_LOCK` 与 `FLAG_KEEP_SCREEN_ON`——都要经 JNI 到达 Activity，而**整个进程里唯一会跟 Java 打交道的地方在 `monitor_gui`**：`keyboard` 持有 VM 与 Activity 的 class 引用，`android` 是它的调用层。`monitor_core` 够不着它，因为依赖方向是 gui → core，反过来不成立。

三个选择，取第三个：

| 方案 | 问题 |
|---|---|
| 在 `monitor_core` 里再写一份 JNI 引导（VM、class 引用、线程 attach、异常清理） | 进程里出现两处"知道怎么跟 Java 说话"的代码，且必须永远同步 |
| 把 Android 分支整个搬到 `monitor_gui` | `apply` / `release` / `status` 被劈成两半，UI 到处 `cfg`，`PowerStatus` 还要复制一份 |
| **`imp(android)` 转发给一个由 GUI 装上的函数指针** | 多一层间接，换来依赖方向与单一职责都保持干净 |

于是 `power.rs` 公开一个类型和一个安装函数：

```rust
/// 由 GUI crate 提供的 Android 实现。
#[cfg(target_os = "android")]
pub type AndroidRequest = fn(system: bool, display: bool) -> Result<()>;

#[cfg(target_os = "android")]
pub fn install_android(request: AndroidRequest);
```

`monitor_gui::app` 在 `App::new` 里装上它，**且在任何人问"这平台能不能保持唤醒"之前**：

```rust
#[cfg(target_os = "android")]
power::install_android(crate::android::set_keep_awake);
```

`is_supported()` 在 Android 上就等于"装上了没有"：

- 装上了 → `true`，面板正常显示两个开关；
- 还没装 → `false`，面板照 §6 置灰并给出说明。

这样"还没实现"与"实现了但此处不可用"共用一个出口，**不需要额外的 gate 开关**：一期在 Android 上自动置灰、二期装上钩子后自动可用，都是这一个判断的结果。

钩子的另一头是 `monitor_gui::android::set_keep_awake`，它用 §5.4 那两个 Java 方法，经一个带参的 JNI 助手调用：

```rust
/// 调用一个带 `boolean` 的 Java 静态方法，签名 `"(Z)V"`。
fn call_void_bool(method: &str, value: bool) -> bool;
```

参数是必须的：`android.rs` 里原有的 `call_void` / `call_bool` 把签名写死成 `"()V"` / `"()Z"`，**不收参数**。它同时按该文件一贯的做法清掉挂起的 Java 异常——留一个挂起异常会让下一次 JNI 调用直接终止进程。

## 5. 各平台实现细节

### 5.1 Windows

手写 FFI，无需新增 crate（现有 `windows-sys` 仅为 `FreeConsole` 引入，此处可直接 `#[link]`）：

```rust
// edition 2024 下 extern 块必须是 `unsafe extern`。
unsafe extern "system" {
    fn SetThreadExecutionState(flags: u32) -> u32;
}
const ES_CONTINUOUS: u32 = 0x8000_0000;
const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;
const ES_DISPLAY_REQUIRED: u32 = 0x0000_0002;
```

- 组合：`prevent_sleep` → `ES_CONTINUOUS | ES_SYSTEM_REQUIRED`；`keep_screen_on` → 再加 `ES_DISPLAY_REQUIRED`。
- 释放：`SetThreadExecutionState(ES_CONTINUOUS)`。
- **判返回值**：失败返回 `0`，应转为 `CoreError` 上报，不要静默当作成功。
- ⚠️ **关键坑**：`ES_CONTINUOUS` 是 **per-thread** 状态。必须在长寿线程（eframe `update` 的主线程）设置与清除，不能丢到临时线程，否则锁随线程结束失效。因此 `apply` / `release` 均从主线程调用。

### 5.2 macOS

外部命令方案，避免 IOKit FFI 复杂度，符合项目既有外部命令风格（`app.rs` 的 `xdg-open`）：

- `prevent_sleep` → `caffeinate -i`（阻止 idle sleep）。
- `keep_screen_on` → 再加 `-d`（阻止 display sleep）。
- **加 `-w <our_pid>`**：`caffeinate -w <pid>` 会在指定进程退出时自行结束。没有它，xgview 崩溃后 `caffeinate` 会变成孤儿，断言一直挂着直到注销。
- 子进程的 stdin/stdout/stderr 一律 `Stdio::null()`。
- 保存 `std::process::Child` handle；`release` 时 `kill()`。

### 5.3 Linux

外部命令 `systemd-inhibit` 持有抑制锁：**子进程存活期间抑制生效，子进程一退出就释放**。

- `prevent_sleep` → `--what=sleep`
- `keep_screen_on` → 再加 `:idle`
- 完整命令：

```text
systemd-inhibit --what=sleep:idle --mode=block --why="xgview monitoring wall" sleep infinity
```

- ⚠️ **不要用 `cat` 当 COMMAND。** `systemd-inhibit` 在 COMMAND 退出时释放锁，而 `cat` 读 stdin 遇到 EOF 就退出——GUI 进程的 stdin 要么是 `/dev/null`（立刻 EOF），要么是终端（`cat` 反而会去抢终端输入）。两种都等于没加锁。用 `sleep infinity`，并把子进程 stdin 设成 `null`。
- ⚠️ **崩溃残留**：`systemd-inhibit` 没有 macOS `-w` 那样的参数，主进程被杀后它会活下来继续持锁，直到注销。要么用 `pre_exec` + `PR_SET_PDEATHSIG`（unsafe、Linux 专有），要么接受这一已知残留并在文档里写明。
- **没有 `xdg-screensaver` 之类的 fallback**，虽然初稿写了一个：它是 X11 工具，在 Wayland 会话里什么都不做，而"开关看起来能用但实际无效"比"面板明确置灰"糟得多。
- ⚠️ **`is_supported()` 必须真的去问一次，不能只看二进制在不在 PATH。** 没有 seat 的环境里 logind 会直接拒绝：`systemd-inhibit` 立刻退出 1 并打印 `Failed to inhibit: Access denied`——容器和 WSL 会话就是这样。只查 PATH 会让开关显示为可用却什么也不做。做法是用一条会立刻结束的命令探测一次（`systemd-inhibit … true`），结果用 `OnceLock` 缓存，因为 `is_supported()` 会被设置面板每帧问到。
- ⚠️ **子进程秒退必须被察觉。** 两个子进程方案都只在子进程存活期间持有锁，所以"起来就死"就等于请求被拒——而原因只写在它的 stderr 上。因此 spawn 之后要看它一小段时间（150 ms）：一旦已经退出就返回错误并带上 stderr，例如 `systemd-inhibit refused the request (exit status: 1): Failed to inhibit: Access denied`。**这不是为了好看**：没有这一步，开关会显示"已生效"而实际什么都没持有——这个缺陷正是在 WSL 上实跑时发现的（`apply` 返回 ok、`status` 显示 `applied = { system: true, display: true }`，而 `systemd-inhibit --list` 里根本没有我们的锁）。macOS 的 `caffeinate` 走同一个 `holder` 助手。

### 5.4 Android（本期：屏幕常亮 + CPU 不睡）

Java 侧 `MainActivity` 加两个**静态单参**方法（不是初稿里的两参方法，理由见 §修订记录 #3），全部走已有形状：

```java
private static PowerManager.WakeLock wakeLock;

/** 保持 CPU 运行（屏幕可关）。权限 WAKE_LOCK 已声明。 */
public static void setPreventSleep(final boolean wanted) {
    final MainActivity self = instance;
    if (self == null) {
        return;
    }
    self.runOnUiThread(() -> {
        if (wanted) {
            if (wakeLock == null) {
                final PowerManager manager =
                        (PowerManager) self.getSystemService(Context.POWER_SERVICE);
                wakeLock = manager.newWakeLock(
                        PowerManager.PARTIAL_WAKE_LOCK, "xgview:wall");
            }
            if (!wakeLock.isHeld()) {
                wakeLock.acquire();
            }
        } else if (wakeLock != null && wakeLock.isHeld()) {
            wakeLock.release();
        }
    });
}

/** 保持屏幕常亮。Activity 级窗口标志，无需权限。 */
public static void setKeepScreenOn(final boolean wanted) {
    final MainActivity self = instance;
    if (self == null) {
        return;
    }
    self.runOnUiThread(() -> {
        if (wanted) {
            self.getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        } else {
            self.getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        }
    });
}
```

- 与 `setReserveNavigationBar` 完全同构：读 `instance`、判空、`runOnUiThread`。**不要**在静态方法里直接调 `getWindow()`。
- 需要新增两个 import：`android.os.PowerManager` 与 `android.view.WindowManager`（`Context` 与 `Window` 已在文件里引用）。
- **怎么被调到**：由 §4.2 的钩子接上 `monitor_core::power`，另一头是 `monitor_gui::android::set_keep_awake`，它用 `call_void_bool` 调这两个方法。两个调用只要有一个没到达活动就返回错误，面板在开关下方显示——一次没生效的唤醒请求不该静默通过。
- 实现里比上面的形状多两处防护：`getSystemService` 可能返回 null（此时直接返回，不建锁）；释放收敛进一个 `releaseWakeLock()` 私有助手，好让 `onDestroy` 与关掉开关走同一句。
- **`onDestroy` 必须补一次释放**：现有 `onDestroy` 只把 `instance` 置空；活动被系统销毁（低内存、旋转以外的重建）时，`WakeLock` 对象会随之失去引用但**锁本身不会释放**，就成了真泄漏。加一句 `if (wakeLock != null && wakeLock.isHeld()) wakeLock.release();`。`FLAG_KEEP_SCREEN_ON` 随窗口自动清除，不需要处理。
- `FLAG_KEEP_SCREEN_ON` 与 `applySystemBars()` 不冲突：后者用的是 `setSystemUiVisibility` / insets，不碰窗口 flag。
- **注意语义**：`prevent_sleep` 而 `keep_screen_on` 关时，屏幕会熄灭、Activity 进入 stopped，CPU 靠 WakeLock 保持运行，RTSP 与解码继续——这正是监控墙"关屏但别断流"的用法。但屏幕已灭，此时 UI 不可见，这一点要在 UI 文案里说清。

## 6. 配置与 UI

`crates/monitor_core/src/config.rs` 的 `AppConfig` 新增两个字段：

```rust
pub prevent_sleep: bool,
pub keep_screen_on: bool,
```

- **不需要 `default_true()`**（该函数不存在，也不必新增）：`AppConfig` 本身带结构级 `#[serde(default)]`，缺字段会回落到 `AppConfig::default()`。只要在 `impl Default for AppConfig` 里写 `prevent_sleep: true, keep_screen_on: true`，旧配置即自动兼容。

> ⚠️ 默认开启意味着普通桌面用户首次运行、以及旧配置升级后，会立即开始阻止睡眠 + 常亮。监控墙场景合理，但必须在 UI 提供关闭入口。

**启动时要打日志。** 用 `tracing::info!` 记下开了哪个开关、用的什么机制。一个用户发现机器不再睡眠时，唯一能指向 xgview 的线索就是这行日志——这与仓库里"日志必须说明原因"的一贯要求一致。

**UI 落点**：System tab（`settings_system`），与 autostart 同段，加两个 checkbox，变更时立即重新 `apply()`。

- **不支持的平台要禁用而不是沉默**：`is_supported()` 为假（或无 systemd）时把 checkbox 置灰，并给出说明——沿用已有的 `settings-autostart-unsupported` 先例与 `decoder_selectable` 的 gate 写法。
- **`apply()` 失败要可见**：把错误显示在该段文字里（或走已有的 toast），不要让用户勾了却什么都没发生。
- **一期在 Android 上先禁用**：一期只有桌面三平台，Android 要等二期；在那之前勾了不生效——这正是"优先硬件解码"在 Android 上那个已知 wart 的同类问题，别再复制一次。

**i18n**（`en.ftl` 与 `langs/zh-CN.ftl` 各加）：

- `settings-prevent-sleep` —— 注意前缀是 `settings-`（System 页），不是初稿里的 `streams-`；
- `settings-keep-screen-on`（文案需体现 §2.1：常亮同时也会阻止系统睡眠）；
- `settings-power-unsupported`；
- 机制串**复用已存在的通用键** `settings-mechanism = mechanism: { $name }`，不必新建 `power-mechanism-*`。

## 7. 生命周期集成

| 时机 | 动作 | 落点 |
|---|---|---|
| 启动 | 按配置 `apply()`，并 `info!` 记录机制 | `App::new` |
| 设置变更 | 重新 `apply()` | System tab checkbox 回调 |
| 桌面退出 | **显式** `release()` | `close_requested` 分支 |
| Android 退出 | **显式** `release()` | `quit()`，且必须在 `std::process::exit(0)` **之前** |
| Android 活动销毁 | Java 侧释放 WakeLock | `MainActivity.onDestroy` |

⚠️ **不要指望 `Drop` 释放。** Android 的 `quit()` 走 `std::process::exit(0)`，**析构函数不会执行**；桌面退出路径同理不可依赖 RAII。`release()` 必须是退出前的一次显式调用——这是硬约束，不是风格选择。

## 8. 风险

- **功耗**：屏幕常亮 + 阻止睡眠显著增加耗电，电池设备尤甚。
- **OLED 烧屏**：静态监控画面长期常亮有烧屏风险。
- **默认开启的行为变更**：旧配置升级后自动开启，需在 UI 显式可关，并在启动日志里留痕。
- **Windows 线程约束**：必须在主线程调用 `SetThreadExecutionState`，否则锁失效。
- **Linux 碎片化 / 抑制被拒**：没有 systemd、或环境本身没有 seat（容器、WSL）时，logind 会拒绝抑制请求。`is_supported()` 通过一次真实探测如实反映，`apply()` 把拒绝原因显示在面板上——两条路都不会静默通过。
- **Linux 孤儿抑制锁**：`systemd-inhibit` 无 `-w` 等价物，主进程崩溃会留下持锁的孤儿进程（macOS 用 `caffeinate -w` 已规避）。
- **Android WakeLock 泄漏**：Java 侧必须成对 acquire/release，且 `onDestroy` 也要释放一次。
- **开关组合的语义**：不加 §2.1 的归一化，"常亮但不阻止睡眠"会产生自相矛盾的状态。
- **不可用平台上的空开关**：`is_supported()` 为假时若不置灰，用户会以为生效了。

## 9. 实施计划

### 一期：桌面三平台

1. 新建 `crates/monitor_core/src/power.rs`：
   - `Applied` + `plan()`（纯函数，带单测）；
   - 平台无关 API（`is_supported` / `mechanism` / `apply` / `release` / `status`）；
   - 三个 `imp`：Windows（`SetThreadExecutionState`，判返回值）、macOS（`caffeinate -i -d -w <pid>`）、Linux（`systemd-inhibit … sleep infinity`）。两个子进程方案一律 `Stdio::null()`。
2. `config.rs`：在 `impl Default` 里加 `prevent_sleep: true` / `keep_screen_on: true`，字段本身加到结构体。
3. `app.rs`：`App::new` 按配置 `apply()` 并 `info!`；`close_requested` 分支显式 `release()`。
4. System tab：两个 checkbox + 变更时重新 `apply()`；不支持的平台置灰并说明；**Android 上本期先置灰**。
5. i18n：`settings-prevent-sleep` / `settings-keep-screen-on` / `settings-power-unsupported`；机制串复用 `settings-mechanism`。
6. `cargo test` + `cargo clippy` 验证。

### 二期：Android

7. `MainActivity.java`：加 `setPreventSleep(boolean)` / `setKeepScreenOn(boolean)`（§5.4）；`onDestroy` 释放 WakeLock。
8. `android.rs`：加 `call_void_bool(method, value)`（签名 `"(Z)V"`，`call_void` / `call_bool` 不收参数）与 `set_keep_awake(system, display)`；`power.rs` 加 `install_android` 钩子（§4.2），`app.rs` 在 `App::new` 里安装它。装上之后 `is_supported()` 即为真，一期的置灰自动消失——**没有单独的 gate 需要放开**。
9. `quit()` 里 `release()`，位于 `std::process::exit(0)` 之前。

### 三期：前台保活（可选，独立评估）

10. manifest 加 `<service>` + `FOREGROUND_SERVICE`（Android 14+ 另加 `foregroundServiceType` 及对应权限）；常驻通知；处理用户的关闭入口。**只有到这一步，UI 才可以宣称"保活"。**

## 10. 验证

- **单元测试**：`plan()` 的归一化（`keep_screen ⇒ system`）与幂等（相同参数返回 `None`）；`is_supported` / `mechanism` 的平台分支。全程不依赖真实系统调用。
- **手动验证**：
  - Windows：`powercfg /requests` 观察 SYSTEM / DISPLAY 请求——**它需要管理员权限，非提升的命令行会被直接拒绝**。拿不到提升权限时读应用自己的日志即可：`xgview::power` 的 `execution state requested` 与 `power management at start-up`（`applied` 为真）。之所以够用，是因为 `SetThreadExecutionState` 成功时**不会**返回 0——实测默认状态下首次调用返回 `0x80000000`（`ES_CONTINUOUS`），所以"返回 0 即失败"的判据成立，日志里没有失败告警就等于请求被接受。若仍要独立观察，把电源计划超时改到 1 分钟直接看是否关屏/睡眠。
  - macOS：`pmset -g assertions` 观察 caffeinate 断言；另需确认 kill 掉 xgview 后 `caffeinate` 也随之消失（验证 `-w` 生效）。
  - Linux：`systemd-inhibit --list` 看抑制锁、`loginctl` 看 idle；再杀掉进程确认锁已释放（否则就是 §8 的孤儿问题）。**在容器或 WSL 里通常拿不到抑制**——那里 `systemd-inhibit` 直接回 `Failed to inhibit: Access denied`、退出 1，于是 `is_supported()` 为 `false`、面板置灰，这是期望行为而不是缺陷；要验"真的抑制住了"得用有 seat 的真 Linux 桌面会话。
  - Android：三条互相独立的证据一起看——`adb shell dumpsys power` 里有 `PARTIAL_WAKE_LOCK 'xgview:wall' (uid=…)`；`adb shell dumpsys window windows` 里我们自己窗口的 `fl=` 含 `KEEP_SCREEN_ON`，且 `mHoldScreenWindow` 指向该窗口；`adb logcat -s xgview` 里 `power management at start-up` 显示 `supported=true` 与 `applied=Applied { system: true, display: true }`。前两条证明的是**效果**，最后一条证明的是钩子装上了、且两个 JNI 调用都成功。另外主动触发活动销毁后确认 WakeLock 没有残留。
