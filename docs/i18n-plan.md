# XGView 多语言（i18n）方案

> 状态：方案归档，暂未实施。
> 选型：Fluent `.ftl` 语言包，运行时加载，可只翻部分并回退英文。

## 1. 背景与现状

当前仓库**没有任何 i18n / locale / 翻译相关代码或依赖**，全部界面文案为英文硬编码，约 **350+ 条**，分布如下：

| 文件 | 约数 | 说明 |
|---|---|---|
| `crates/monitor_gui/src/app.rs` | 200+ | 设置面板、状态栏、toast、About、键盘表 |
| `crates/monitor_gui/src/dialogs.rs` | 120+ | Add devices 三个 tab、字段名、校验消息 |
| `crates/monitor_core/src/model.rs` | ~30 | 枚举 `label()/as_str()/tag()`，被全 UI 引用 |
| `crates/monitor_gui/src/grid.rs` | ~15 | tile OSD 文案 |
| `crates/monitor_core/src/error.rs` | 10 | `thiserror` 错误前缀 |
| `src/main.rs` | ~15 | HELP、命令行输出 |
| `layout.rs` / `discovery/mod.rs` / `autostart.rs` | 若干 | 隐式上屏文案 |

**字体现状**：`crates/monitor_gui/src/fonts.rs` 已从系统加载 UI / Mono / CJK 三类字体，CJK 面追加到字体族末尾兜底，中文/日文/韩文字形可正常渲染。**唯一细节风险**：`CJK_FACES` 只取 Noto CJK 的 **SC（简体）字形面**（`fonts.rs:113/120/122/123` 的 `index: 2`），切到日文时同源汉字会显示简体字形。

**配置现状**：`crates/monitor_core/src/config.rs:154` 的 `AppConfig` 为 serde + JSON，类上有 `#[serde(default)]`，新增字段对旧 `config.json` 完全兼容，无需迁移、无需升 `CONFIG_VERSION`。

## 2. 目标与约束

- 默认英文；界面文案与代码解耦。
- 语言以**外部文件**形式附加，放进目录即生效，新增语言**无需重新编译**。
- 语言包可只翻一部分，缺失项自动回退英文，最终回退到 key 本身。
- 中文/日文/韩文字形正确（按语言选字体面）。
- 不引入渲染循环内的可感开销。

## 3. 技术选型

使用 **Fluent（`.ftl`）**，理由：

- 纯文本，非程序员可直接编辑、审阅。
- 原生支持变量内插、语序重排、CLDR 复数，避免 `format!` 直拼导致的语序错误。
- 运行时可从任意文件加载，天然满足“附加语言包”诉求。
- 提供 fallback 链机制。

新增依赖（纯 Rust，无 C 依赖）：

```toml
fluent = "0.16"
fluent-bundle = "0.15"
unic-langid = "0.9"
sys-locale = "0.3"   # 仅在 language = "auto" 时用于探测系统语言
```

## 4. 总体架构

新增独立 crate `monitor_i18n`（加入根 `Cargo.toml` 的 `[workspace] members`）：

```
monitor_i18n
  ├─ catalog.rs   全局目录：当前 bundle + en 兜底 bundle + 可用语言列表
  ├─ loader.rs    发现/解析 langs/*.ftl，内嵌 en
  ├─ plural.rs    复数与变量格式化封装
  └─ lib.rs       tr() / tr_args() / init() / set_language() / available()
```

**核心约定：`monitor_core` 不再返回成品文案，只返回稳定的 key。**

- `model.rs` 的 `StreamKind::label()`、`ConnectionState::label()`、`CameraOrigin::label()`、`TileAspect::label()`、`OsdItem::label()` 等改为返回 key（或保留枚举、由 UI 层翻译）。
- `error.rs` 的 `thiserror` 文案前缀 key 化；底层 `{0}` 仍是 `std::io` / `reqwest` 的英文，不强行翻译。
- 好处：后台线程不依赖全局翻译状态；语序/复数完全交给语言包处理。

## 5. 语言包格式与目录

示例 `zh-CN.ftl`：

```ftl
### name: 简体中文
### code: zh-CN

add-devices = 添加设备
save = 保存
cancel = 取消

cameras-count =
    { $count ->
        [one] { $count } 台相机
       *[other] { $count } 台相机
    }

remove-confirm = 删除 “{ $name }”？
discovery-done = 发现完成：{ $onvif } 台 ONVIF 设备，{ $ports } 个开放端口
```

**目录与优先级**（后者覆盖前者）：

1. 内嵌 `en.ftl`（`include_str!`，永不缺失的兜底）。
2. `monitor_core::config::config_dir()/xgview/langs/*.ftl`（用户级）。
3. `<exe_dir>/langs/*.ftl`（随分发包附加）。

文件名即 BCP-47 语言 tag（`en`、`zh-CN`、`ja`、`zh-TW`）；`### name:` 注释供设置面板显示本地化语言名。

**附加语言包** = 把 `zh-CN.ftl` 复制进 `langs/` 目录，重启即出现在语言列表。

## 6. 运行时 API

```rust
pub fn init(wanted: &str);                    // "en" | "zh-CN" | "auto"
pub fn tr(key: &str) -> String;               // 无变量
pub fn tr_args(key: &str, args: &[(&str, ArgValue)]) -> String;
pub fn available() -> Vec<LanguageInfo>;      // { id, native_name, coverage_percent }
pub fn set_language(id: &str) -> bool;        // 切换，返回是否变化
```

- 查找顺序：当前语言 → `en` → 返回 key 字符串本身（并只告警一次，便于发现漏翻）。
- 全局状态用 `thread_local!`（egui 全在 UI 线程），无锁、零原子开销。
- 调用点改动最小：`ui.label("Add devices")` → `ui.label(tr("add-devices"))`。
- `"auto"` 用 `sys-locale` 探测系统语言。

## 7. 配置与切换

`crates/monitor_core/src/config.rs:154` 的 `AppConfig` 新增：

```rust
#[serde(default = "default_language")]
pub language: String,   // 默认 "en"
```

- 旧 `config.json` 缺该字段时自动取默认，**无需版本迁移**。
- `language` 随导出/导入走；“Only import the cameras” 分支不会覆盖它（符合预期）。
- 设置面板新增 **Language** 区（建议放 Display tab 顶部或 About 旁），用 `ComboBox` 列出 `available()`。
- 切换流程：`set_language()` → `ctx.request_repaint()` → 重新 `fonts::install_for(ctx, lang)`。

## 8. 字体联动

将 `fonts.rs` 的 `install(ctx)` 改为 `install_for(ctx, lang)`：

- 拉丁 UI 面、Mono 面不变。
- CJK 候选表按语言选取字形面：Noto CJK TTC 顺序为 JP=0 / KR=1 / SC=2 / TC=3 / HK=4；Windows 无 Noto CJK 时分别用 `msgothic`（日）/ `malgun`（韩）/ `msjh`（繁）。
- 切换语言时重新安装字体，避免同源汉字显示错误字形。

## 9. 变量 / 复数 / 语序（必须专门处理的清单）

**复数**（英文用 `(s)` 偷懒，其它语言必须 ICU plural）：

- `app.rs:1746` `format!("{live}/{} live", ...)`
- `app.rs:1803` `format!("Cameras ({})", ...)`
- `dialogs.rs:269-273` `format!("discovery finished: {} ONVIF device(s), {} open port(s)", ...)`
- `dialogs.rs:289` `format!("{} camera(s) on the NAS ...", ...)`
- `dialogs.rs:543` `format!("{targets} target address(es)")`
- `dialogs.rs:605` `format!("Other open ports ({})")`
- `main.rs:130-137` `format!("... {} live channel(s) [{}]", ...)`

**语序 / 句子拼接**（需重构成整句 key）：

- `app.rs:2648-2662` 版本 / 作者 / 版权拆成三个 label → 语序无法整体翻译。
- `app.rs:2712-2720` `import_hint()` 的 `{body}` 内插与拼接。
- `app.rs:2595` `format!("Remove \"{name}\"?")` 引号包裹变量。
- `dialogs.rs:869-872` `format!("{} {name}", if ... {"updated"} else {"added"})` 动词 + 名词拼接。

**宽度相关**：

- `app.rs:2603` `center_offset(ui, &["Keep", "Remove"])` 依赖字符串宽度做居中；翻译后按新宽度自动计算即可。

**动态来源（翻译表需覆盖 core）**：

- `model.rs:21-37/69-74/96-98/122-123/190-205/269-276` 各 `label()/as_str()/tag()`。
- `layout.rs:49-54`（`"1x1"…"4x4"` 纯数字不译）。
- `discovery/mod.rs:99/113/133` 阶段文案。
- `autostart.rs:55-67/135` mechanism。

**含特殊符号**：`app.rs:2910` 的弯引号 `“Add devices”`、`app.rs:1753` 的 `·`、`app.rs:2642` 的 `· ©`，本地化时统一处理。

## 10. 性能

- FTL 解析只在加载时发生一次；每帧仅 hash 查表 + 极轻量格式化。
- UI 每秒仅数十次字符串调用，相对 wgpu 渲染开销可忽略。
- 无每帧锁、无分配热点；`tr` 可返回 `Cow` 进一步减少分配（非必需）。
- 语言切换是一次性重载，不进入渲染循环。

## 11. 迁移阶段（建议顺序）

1. 新增 `monitor_i18n` crate：内置 `en.ftl`、全局 API、完善 `tr` 回退与告警。
2. `AppConfig` 加 `language` 字段 + 默认值。
3. 抽 `app.rs`（含状态栏 / toast / About / 键盘表）。
4. 抽 `dialogs.rs`、`grid.rs`。
5. core 枚举 key 化：`model.rs`、`layout.rs`、`discovery/mod.rs`、`autostart.rs`。
6. `error.rs` 前缀 key 化（底层英文不动）。
7. 设置面板 Language 选择 + `fonts::install_for` 字体联动。
8. `main.rs` 的 HELP / 命令行输出（可选，视是否面向终端本地化）。
9. 发布 `zh-CN.ftl` 作为首个附加语言，全链路验证。

## 12. 风险与注意

- **打包**：需要把 `langs/` 目录随桌面/Android 分发包一起复制到可执行文件旁；Android 下建议同时支持 `config_dir()/langs`。
- **覆盖率显示**：语言包可部分翻译，`available()` 中展示 `coverage_percent`，避免用户以为没生效。
- **字体缺失**：极端精简系统若没有任何 CJK 面，非拉丁文案仍显示为方框（现有行为，`fonts.rs:180` 已有告警）。
- **底层错误英文**：`error.rs` 的 `{0}` 来自 std / reqwest，彻底本地化成本高，方案只译前缀。
- **relayout**：切语言后长句宽度变化，`request_repaint` 必须触发，工具栏自适应缩放（`app.rs:1605-1620`）需实测。
## 13. 发布目录结构

### 核心原则

- **官方语言包编进二进制**（`include_str!` 内嵌 en 及随版本发布的语言），保证任何发布形态、任何目录都可用。
- **附加语言包走外部目录**（纯 `.ftl`），放进即生效、即出现、即覆盖同名内置语言。

### 源码仓库布局

```
xgview/
├─ langs/                         官方语言包源文件，由构建脚本复制进发布目录
│  ├─ en.ftl                      模板 + 与内嵌一致，供翻译者参照
│  ├─ zh-CN.ftl
│  └─ ja.ftl
├─ crates/monitor_i18n/
│  ├─ src/...
│  └─ langs/en.ftl                编译期内嵌的兜底（include_str!）
└─ scripts/                       需新增 langs 复制逻辑
```

### Windows 发布包 / 安装目录

zip 解压后（或 `install-windows.ps1` 的 `%LOCALAPPDATA%\XGView\`）：

```
xgview-1.1.12-x64\                 (= %LOCALAPPDATA%\XGView\)
├─ xgview.exe
├─ avcodec-63.dll / avformat-63.dll / avutil-63.dll / swscale-*.dll / swresample-*.dll
└─ langs\
   ├─ en.ftl
   ├─ zh-CN.ftl
   ├─ ja.ftl
   └─ <用户/第三方新增>.ftl
```

用户级附加与覆盖（最高优先级）：

```
%APPDATA%\xgview\
├─ config.json                    含 "language": "zh-CN"
└─ langs\
   └─ zh-TW.ftl
```

### Android

官方语言（en）内嵌进 `libmonitor_android.so`；附加语言随 APK assets 分发，启动时由 `extract_lang_assets` 提取到 `config_dir/langs/`（即 `internal_data_path/xgview/langs/`）：

```
APK
└─ assets/
   └─ zh-CN.ftl                   Gradle 从仓库 langs/ 打入

/data/data/com.xhbl.xgview/files/xgview/   config_dir（internal_data_path/xgview）
├─ config.json
└─ langs/
   └─ zh-CN.ftl                   启动时从 assets 提取（每次覆盖，保证更新）

/storage/emulated/0/Android/data/com.xhbl.xgview/files/   external_data_path
└─ langs/
   └─ ja.ftl                      adb push 的用户附加包（最高优先级）
```

机制：
1. `android/app/build.gradle.kts` 的 `sourceSets.main.assets.srcDirs("../../langs")` 把仓库 `langs/*.ftl` 打进 APK assets。
2. `monitor_gui::run_android` 启动时调 `extract_lang_assets`，用 `ndk::asset::AssetManager` 遍历 assets 根，把每个 `.ftl` 写到 `config_dir/langs/`。
3. `monitor_i18n::init` 已搜索 `config_dir/langs/`，自动发现。`external_data_path/langs/` 仍可 `adb push` 覆盖（优先级更高）。

### Linux / macOS

```
/usr/bin/xgview                    二进制（官方语言内嵌）
/usr/share/xgview/langs/*.ftl      发行包语言（只读，可选）
~/.config/xgview/
├─ config.json
└─ langs/*.ftl                     用户附加（最高优先级）
```

### 运行时搜索与优先级

查找顺序（后者覆盖前者同名 key / 同名语言文件）：

| 平台 | 语言包搜索路径 |
|---|---|
| 全部 | 内嵌（en 兜底 + 随版本官方语言） |
| 桌面 | `<exe_dir>/langs/` → `config_dir()/xgview/langs/` |
| Android | `config_dir()/langs/`（assets 提取）→ `external_data_path()/langs/`（adb push，优先级更高） |

缺失 key 的解析：当前语言 → `en` → 返回 key 本身（告警一次）。

### 需要改动的脚本点

- `scripts/build-windows.ps1:128-137`：staging 阶段把 `langs\` 复制到 `$stage\langs\`。
- `scripts/install-windows.ps1:128-148`：把 `langs\` 复制到 `$InstallDir\langs\`，并创建 `$configRoot\langs`。
- `scripts/build-android.ps1`：无需改动（Gradle assets 自动打包）。
- `android/app/build.gradle.kts`：`sourceSets.main.assets.srcDirs("../../langs")` 把语言包打进 APK。
- `crates/monitor_gui/src/lib.rs`：`extract_lang_assets` 在 `run_android` 启动时把 assets 提取到 `config_dir/langs/`。
- 新增仓库 `langs\` 并纳入版本管理。