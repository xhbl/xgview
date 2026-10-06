### XGView 语言包：简体中文。
###
### 每个键在英文（内嵌）里有对应值；此处未翻译的键会回退到英文。
### 键为 Fluent 消息标识符（字母、数字、`-`），不含点号。
###
### `name:` 行用本语言文字写出语言名，显示在设置面板里。

### name: 简体中文

app-name = XGView
greeting = 你好，{ $name }！
ok = 确定
cancel = 取消
save = 保存
close = 关闭

## 设置面板
settings = 设置
settings-language = 语言
settings-language-auto = 自动（跟随系统）
settings-language-hint = 界面语言。将 .ftl 文件放入可执行文件旁的 langs 文件夹（或配置目录下），新语言即会出现在此处。
settings-tab-display = 显示
settings-tab-cameras = 摄像头
settings-tab-streams = 流
settings-tab-system = 系统
settings-tab-about = 关于
settings-tab-cameras-count = 摄像头 ({ $count })
settings-grid = 宫格
settings-osd = 屏显
settings-osd-hint = 每个格子四角显示的内容。按按钮循环切换。
settings-fullscreen = 全屏（F11）
settings-start-fullscreen = 启动时以全屏打开
settings-reserve-navigation-bar = 保留导航栏
settings-reserve-navigation-bar-hint = 勾选：导航栏占位、画面变窄；取消：沉浸式全屏。无导航栏时无效果。
corner-top-left = 左上
corner-top-right = 右上
corner-bottom-left = 左下
corner-bottom-right = 右下

## 设置 / 系统
settings-startup = 启动
settings-autostart = 随系统开机启动
settings-autostart-unsupported = 此平台不支持开机自启
settings-mechanism = 机制：{ $name }
settings-keys = 按键
key-move-focus = 移动焦点
key-enter = 确认 / 选择 / 缩放
key-back = 返回 / 取消
key-grid = 切换宫格
key-page = 上一页 / 下一页
key-settings = 设置
key-add-devices = 添加设备
key-fullscreen = 全屏

## 设置 / 流
streams-hint = 多画面宫格拉取辅码流，放大视口拉取主码流。
settings-reconnect = 重连
streams-first-retry = 首次重试于
streams-then-at-most = 其后至多
streams-change-note = 更改仅对之后建立的连接生效
streams-backoff = 重连退避策略
streams-factor = 因子
streams-jitter = 抖动
streams-attempts = 尝试次数（0 = 无限）
settings-decoding = 解码
streams-prefer-hardware = 优先硬件解码
streams-summary-fixed = 当前此处只能使用硬件解码，无其它可用，因此无需切换。
streams-summary-available = 若设备支持该视频流的硬件解码，则由 GPU 解码；其余情况均由 CPU 解码。每个画面均会显示当前所使用的解码方式。
streams-summary-software = 当前此处未集成硬件解码器：无论此项如何设置，均由 CPU 进行软件解码。

## 摄像头
cameras-confirm-order = 确认顺序
cameras-add-devices = 添加设备…
cameras-add-manually = 手动添加…
cameras-sub-derived = 辅流：推断
cameras-sub-set = 辅流：已设
cameras-infer-sub = 推断辅流
cameras-transport-tip = 此摄像头的 RTSP 传输方式。UDP 可绕过损坏 TCP 交叉帧的中继。
cameras-remove-tip = 移除此摄像头
cameras-edit-tip = 编辑此摄像头
cameras-edit-name = 编辑摄像头
cameras-remove-name = 移除摄像头
cameras-aspect-tip = 画面如何适配格子
cameras-aspect-name = 显示比例
cameras-reorder = 重排摄像头
cameras-move-down = 下移
cameras-move-up = 上移
cameras-move-camera-up = 上移摄像头
cameras-move-camera-down = 下移摄像头
camera-edit-title = 编辑摄像头
camera-edit-sub-derived = 辅码流地址由主码流推断

## 移除确认
remove-title = 移除“{ $name }”？
remove-hint = 它将离开大屏和配置；摄像头本身不受影响。
action-keep = 保留
action-remove = 移除

## 关于
about-description = 监控摄像头网格查看器
about-title = { $app }：{ $description }
about-version-by = v{ $version } 作者
about-copyright = · © { $years }
about-decoder = 解码器：{ $backend }（{ $mode }）
about-decoder-hardware = 硬件解码可用
about-decoder-software = 仅软件解码
about-config = 配置：{ $path }
about-export = 导出…
about-import = 导入…
about-author-name = 作者
about-repository = GitHub 项目主页
about-export-name = 导出配置
about-import-name = 导入配置
about-only-cameras = 仅导入摄像头
about-only-cameras-name = 仅导入摄像头
about-import-hint = 导入会覆盖当前配置。勾选此框仅导入摄像头，保留本查看器自身的设置。
about-import-hint-android = 导出保存到 Download/xgview/config.json。导入会打开系统文件选择器。{ about-import-hint }

## 工具栏
toolbar-layout-1x1 = 宫格 1×1
toolbar-layout-2x2 = 宫格 2×2
toolbar-layout-3x3 = 宫格 3×3
toolbar-layout-4x4 = 宫格 4×4
toolbar-back-grid = 返回宫格
toolbar-back-grid-tip = 返回宫格（Esc / 返回）
toolbar-prev-page = 上一页
toolbar-next-page = 下一页
toolbar-page = 第 { $page } / { $count } 页
toolbar-fullscreen-enter = 全屏
toolbar-fullscreen-leave = 退出全屏
toolbar-fullscreen-enter-tip = 全屏（F11）
toolbar-fullscreen-leave-tip = 退出全屏（F11）
toolbar-settings = 设置
toolbar-settings-tip = 设置（F1）
toolbar-add-devices = 添加设备
toolbar-add-devices-tip = 添加设备（F2）

## 状态栏
status-live = { $live }/{ $total } 在线
status-page = { $layout } · 第 { $page }/{ $count } 页
status-zoom = 1x1 放大 · 主码流
status-focus = 焦点 #{ $index }
status-no-focus = 无焦点

## 大屏与退出
wall-empty-title = 未配置摄像头
wall-empty-hint = 点击 "+" 或按 F2 扫描网络摄像头或手动添加
exit-hint = 再按一次返回退出

## 提示
toast-autostart-on = 已注册开机自启
toast-autostart-off = 已移除开机自启注册
toast-autostart-failed = 开机自启：{ $error }
toast-decoding-gpu = 通道重开，GPU 尽力解码
toast-decoding-cpu = 通道在 CPU 上重开
toast-no-sub = 未识别辅码流模式
toast-reorder = 重排：上 / 下移动 · 确认应用 · 返回取消
toast-config-save-failed = 无法保存 { $path }：{ $error }
toast-config-exported = 配置已导出到 { $path }
toast-export-failed = 无法导出：{ $error }
toast-export-needs-storage = 导出需要存储权限——授权后再导出一次
toast-imported = 已从 { $path } 导入 { $count } 个摄像头
toast-imported-document = 已从所选文件导入 { $count } 个摄像头
toast-import-failed = 无法导入：{ $error }
toast-started-autostart = XGView { $version } 已自动启动（{ $decoder } 解码器）
toast-started = XGView { $version } — { $decoder } 解码器
toast-camera-added = 已添加 { $name }
toast-camera-import-failed = { $name }：{ $error }
toast-synology-found = 在 Surveillance Station 上找到 { $count } 个摄像头
toast-synology-failed = Synology：{ $error }
toast-discovery-failed = 发现失败：{ $error }
status-copyright = © { $years }
status-decoder = 解码器：{ $backend }

## Android 开机自启
android-boot-hint = Android 除非系统允许，否则拒绝从开机广播启动应用。以下任一在设备上设置一次即可。
android-home-button = 主屏应用…
android-home-name = 主屏应用设置
android-home-yes = XGView 是主屏应用
android-home-no = XGView 不是主屏应用
android-overlay-button = 在其他应用上方显示…
android-overlay-name = 在其他应用上方显示的设置
android-overlay-yes = 已允许
android-overlay-no = 未允许
android-adb-hint = 或用 adb 从电脑设置：{ $nl }adb shell appops set com.xhbl.xgview SYSTEM_ALERT_WINDOW allow

## 枚举标签（由 core 返回，UI 翻译）
stream-kind-main = 主码流
stream-kind-sub = 辅码流
stream-tag-main = 主
stream-tag-sub = 辅
decode-hw = HW
decode-sw = SW
state-idle = 空闲
state-suspended = 已挂起
state-connecting = 连接中...
state-streaming = 在线
state-reconnecting = 重连中...
state-failed = 失败
origin-manual = 手动
origin-onvif = ONVIF
origin-synology = Synology
aspect-original = 原始
aspect-stretch = 拉伸
aspect-16x9 = 16:9
aspect-4x3 = 4:3
aspect-1x1 = 1:1
aspect-short-original = 原始
aspect-short-stretch = 填满
aspect-short-16x9 = 16:9
aspect-short-4x3 = 4:3
aspect-short-1x1 = 1:1
osd-off = 无
osd-name = 名称
osd-number-name = 编号 + 名称
osd-stream = 在线标识 + 码流
osd-link = 传输 + 码率
osd-fps = 帧率
osd-format = 尺寸 + 比例
osd-detail = 所有状态信息
discovery-source-multicast = WS-Discovery（多播）
discovery-source-unicast = WS-Discovery（单播）
discovery-source-portscan = TCP 端口扫描
autostart-mechanism-windows = 注册表（HKCU Run）
autostart-mechanism-xdg = XDG 自启（.desktop）
autostart-mechanism-android = Android BootReceiver（RECEIVE_BOOT_COMPLETED）
autostart-mechanism-unsupported = 不支持

## 宫格格子状态
grid-stream-failed = 码流失败
grid-suspended = 已挂起
grid-switching-to = 切换到 { $stream }…
grid-holding-last = 保留上一 { $tag } 帧
grid-waiting-video = 等待画面

## 添加设备对话框
dialog-add-devices = 添加设备
dialog-tab-onvif = ONVIF / 网络扫描
dialog-tab-manual = 手动输入
dialog-tab-synology = Synology NAS
dialog-close = 关闭
dialog-probe = 探测 239.255.255.250:3702
dialog-full-scan = 完整扫描（WS-Discovery + TCP）
dialog-scan-settings = 扫描设置
dialog-multicast-probe = 多播探测（本地子网）
dialog-unicast-probe = 对下列网段单播探测（VLAN / 跨子网）
dialog-ip-ranges-hint = 每行一个，如 192.168.1.1-254 或 10.0.0.0/24
dialog-ip-ranges = IP 网段
dialog-target-count = { $count } 个目标地址
dialog-tcp-fallback = TCP 回退扫描
dialog-tcp-ports = TCP 端口
dialog-probe-timeout = 探测超时（ms）
dialog-concurrent-probes =  并发探测数
dialog-onvif-credentials = ONVIF 凭据（用于 GetProfiles / GetStreamUri）
dialog-user = 用户
dialog-password = 密码
dialog-onvif-devices = ONVIF 设备（{ $count }）
dialog-no-device = 暂无设备响应
dialog-add = 添加
dialog-added = 已添加
dialog-update = 更新
dialog-other-ports = 其他开放端口（{ $count }）
dialog-other-ports-hint = 未应答 WS-Discovery 的设备；用 RTSP 地址手动添加
dialog-use = 使用
dialog-open-port = 开放端口
dialog-name = 名称
dialog-main-stream = 主码流
dialog-sub-stream = 辅码流
dialog-sub-hint = 可选，由主码流推断
dialog-infer = 推断
dialog-derive-sub = 辅码流为空时由主码流推断
dialog-add-camera = 添加摄像头
dialog-clear = 清空
dialog-camera-added = 摄像头已添加
dialog-sub-derived-msg = 辅码流地址由主码流推断
dialog-synology-hint = SYNO.API.Auth + SYNO.SurveillanceStation.Camera：拉取 NAS 上每个摄像头的主辅码流。
dialog-host = 主机
dialog-port = 端口
dialog-scheme = 协议
dialog-https = https
dialog-account = 账户
dialog-fetch-cameras = 获取摄像头
dialog-contacting-nas = 正在联系 NAS…
dialog-cameras-on-nas = NAS 上的摄像头（{ $count }）
dialog-nothing-fetched = 尚未获取
dialog-sub-tag = 辅流
dialog-already-added = 已添加，设置不同
dialog-updated-msg = 已更新 { $name }
dialog-added-msg = 已添加 { $name }
dialog-discovery-finished = 发现完成：{ $devices } 个 ONVIF 设备，{ $ports } 个开放端口
dialog-imported = 已导入 { $name }（{ $address }）
dialog-synology-count = NAS 上 { $count } 个摄像头——选择要添加的
dialog-starting = 启动中…
dialog-an-rtsp-url-required = 需要 RTSP 地址
dialog-url-must-start = 地址必须以 rtsp:// 开头

## 发现摘要（设置面板）
summary-multicast = 多播
summary-unicast = 单播（{ $count } 个网段）
summary-tcp = tcp { $ports }
summary-disabled = 已禁用

## 浮层输入框（Android 软键盘）
input-select-all = 全选
input-copy = 复制
input-paste = 粘贴
input-clear = 清空
input-done = 完成