# OpenScreen-rs

把屏幕共享出去，在另一台电脑（或手机）的浏览器里看。同一个软件想法的 Rust 版本，两种模式：

| | MJPEG（默认） | H.264（`--h264`） |
|---|---|---|
| 抓屏 | DXGI 桌面复制（本程序） | ffmpeg 的 `ddagrab` |
| 编码 | libjpeg-turbo，4:4:4 | NVENC 硬件编码 |
| 解码 | 浏览器原生 `<img>` | WebCodecs（走硬件解码） |
| 1080p60 带宽 | **75 Mbps**（9.4 MB/s，163 KB/帧） | **3.3 Mbps** |
| 帧率（实测） | 58–62 fps | 66 fps |
| 编码 CPU（整机） | 1.7% | **0.4%** |
| 依赖 | 只要 `turbojpeg.dll` | 还要 `ffmpeg.exe` |
| 需要 HTTPS/localhost | 不需要 | WebCodecs 需要；`--mse` / `--webrtc` 不需要 |

还有第三种传输：**`--webrtc`**，H.264 走 RTP/UDP，没有分片量化、没有缓冲层，是这套程序能达到的最低延迟。
实测（本机回环）：**60 fps、rtt 0–4 ms、jitter 4–7 ms、丢包 0**。它同样不受安全上下文限制，`http://IP` 直接可用。
代价是依赖：WebRTC 需要 tokio + webrtc-rs，exe 从 489 KB 变成 **5.8 MB**，"零依赖"这个前提因此放弃。

### 默认就是原生分辨率

`--height` 默认 **0 = 原生**：屏幕多大就发多大，不做任何缩放。缩放是唯一不可逆的损失，
而且跳过它还能让帧留在 GPU 上直接进编码器（实测 1600p 从 54 fps 提升到 58.8 fps）。
只有显式给 `--height 1080` 之类才会降采样。

带宽参考（2560x1600@60）：WebRTC 约 16–20 Mbps、MSE 约 12 Mbps、MJPEG 150 Mbps 以上。

### 高刷新率屏幕：`--fps 120`

手机/平板的屏幕常常是 120 Hz。这时 `--fps 60` 看到的帧率数字会显示 **119**，但那是**呈现帧率**
（每一帧被显示两次），不是我们发得多。想要真正的 120 帧：

```powershell
--fps 120 --bitrate 24000
```

实测这台机器能撑住：**2560x1600 @120 = 600 帧/5 秒、24 Mbps**，端到端解码 **119 fps**、丢包 0。
代价是带宽翻倍（120 fps × 1600p 约 24 Mbps），手机热点/弱 WiFi 上要谨慎；1080p 120 fps 大约 12 Mbps。

### 接了外接显示器后抓屏失败

这是最容易踩的坑：抓屏默认抓 `output_idx=0`，但**插上第二块屏之后，能复制的往往不是 0 号输出**，
于是 `DuplicateOutput` 返回 `DXGI_ERROR_UNSUPPORTED`（0x887A0004），ffmpeg 直接
`Selected output not supported`，编码器起不来 —— 表现在页面上就是"状态栏有数字但全是 0、画面全黑"，
非常像对端不兼容。

现在两条路径都改成**逐个 output 试复制，谁能用就用谁**（优先虚拟桌面原点所在的那个），并把选中的
索引传给 ffmpeg 的 `output_idx`。

### 连接成功但没有画面（严格的对端）

如果某个客户端能建立连接、状态显示 `connected`、但 `0 fps`，最可能是**它拒绝了我们声明的 H.264
profile**。iOS Safari 对 profile 比较严格，可以换一个再试：

```powershell
--profile baseline    # 兼容性最好（42xx）；默认是 high（64xx）
--profile main        # 折中（4Dxx）
```

SDP 里的 `profile-level-id` 会跟着变，和编码器实际输出保持一致。**它必须是 3 字节 = 6 位十六进制**
（profile idc、约束标志、level），写成 4 位（如 `642A`）属于非法属性，严格的接收端会直接拒掉整条
媒体段——表现就是"连接成功但没有画面"。现在三个值分别是 `64002A` / `4D402A` / `42E02A`，
并有一个单元测试锁住这个格式。

### 网络限制：UDP 被屏蔽怎么办

ICE 同时提供 **UDP 和 TCP 候选**（实测 10 + 20 个）。TCP 是 `tcptype passive`，浏览器主动连进来，
走的就是 HTTP 同一条通路，所以能打开页面的网络就能建立媒体连接。

已知案例：校园网（`stu-xdwlan`）**屏蔽客户端之间的 UDP**，那里 WebRTC 连不上，而 `--mse`（纯 TCP）可以。
这时要么用手机热点，要么退回 `--mse`。

另外，浏览器上报的候选常常是 **mDNS 名字**（`xxxx.local`），Chrome/Safari 为隐私把内网 IP 混淆了。
我们的候选是真 IP，所以由浏览器发起连接即可；服务端会把这行候选打印出来，连不上时据此排查。

两种模式共用同一套东西：单生产者 + 只发最新帧 + 自适应码率 + **CSS 光标叠加层**。

## 远程控制（鼠标 / 键盘）

默认**关闭**（看画面是被动的，动别人的键盘不是），在控制面板勾上「允许观看端操作这台电脑」才生效。
实现用的是 Windows 自己的 `SendInput`，没有引入任何依赖。

| 项目 | 做法 |
|---|---|
| 通道 | 复用光标那条 `/ws`，一条消息 10–20 字节（`m x y` / `b 按键 0\|1` / `w 滚轮` / `k 键码 0\|1` / `r` 全松开） |
| 坐标 | 观看端发的是**画面的比例**（0–10000），不是像素；服务端按「正在共享的那块屏」的矩形换算，再归一化成 `SendInput` 要的 0–65535 |
| 键盘 | 发浏览器的 `event.code`（物理键），服务端映射成 Set 1 扫描码加 E0 前缀，所以两边的键盘布局可以不一样 |
| 多屏 | 只有共享的那块屏参与换算：点的是哪块屏，就落在哪块屏 |

实测（2560x1600 主屏，落点由程序内部读回，不是靠截图猜的）：

```
m 5000 5000   -> (1280, 800)    正中心，逐像素一致
m 2500 7500   -> (640, 1200)
m 0 0         -> (0, 0)
```

两个不写下来一定会再踩的坑：

- **DPI 必须显式打开**（启动时 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`）。这台机器是
  125% 缩放，不打开时 `GetSystemMetrics` 说桌面是 2048x1280，而 DXGI 和光标位置都说 2560x1600：
  注入的坐标会落在期望位置的 80% 上；而且「原生分辨率」实际上被悄悄截到 1280p。打开之后两个空间
  合一，顺带修掉了这个截断。
- **虚拟桌面的尺寸要问 `SM_*VIRTUALSCREEN`，不能用 DXGI 输出拼出来**。枚举是按显卡走的，核显带一块
  屏、独显带另一块时，拼出来的包围盒和 `SendInput` 除的那个数不是同一个，位置会整体缩水。

另外：观看端离开、页面失焦、关闭控制，都会把按下的键和鼠标键全部松开（服务端记住按过哪些），
不然对方会留着一个按住的 Ctrl。

限制（Windows 本身的，不是没写）：UAC 提权窗口注入不进去；中文输入法的候选词进不去（英文、数字、
快捷键都正常）；浏览器自己的快捷键抢不回来（`Ctrl+W` 还是关标签页）。控制通道**目前没有密码**，
所以只建议在可信的局域网里开启；运行窗口里输入 `x` 回车可以立刻断开。

## 单帧成本（MJPEG 模式，2560x1600 屏 → 1728x1080 帧，4:4:4）

| 环节 | 做法 | 实测 |
|---|---|---|
| 抓屏 | DXGI Desktop Duplication，GPU 内复制 + staging 回读 | 2.8–3.3 ms |
| 缩放 | 面积平均，6 线程（内存密集，线程再多反而更慢） | 4.3 ms |
| 编码 | libjpeg-turbo，质量 70，4:4:4 | 4.9 ms |
| **合计** | | **12.5 ms（≈80 fps 上限）** |

同一台机器、同一套原生 API 的 C# 版是 9.7 ms；纯 JDK 的 Java 版是 97 ms（10 fps）。

**这个 fps 是"管线上限"，不是看到的帧率**——桌面不动的时候一帧都不会产生（DXGI 会说
"没变化"），我们也就不会白费 CPU 去编码和发送。

## 画质

早期版本看起来"糊"，是两个真实缺陷叠出来的，不是参数没调好：

1. **缩放其实近似最近邻**：旧代码把覆盖范围取整，2560→1728（比例 1.48）时有的目标像素只平均
   1 个源像素、邻居平均 2 个。现在改成**按分数覆盖权重的面积平均**（权重 1/256 定点，整数运算）。
2. **色度用 4:2:0**：色度分辨率被砍一半，彩色文字边缘发糊。现在默认 **4:4:4**。
3. 另外自适应的触发阈值从 20 ms 放宽到 60 ms（原来一次卡顿就会把质量永久压到 45），恢复速度 +1 → +2。

实测（同一张固定测试图，3 倍放大裁切，见 `docs/quality-before-after.png`）：

| | 帧大小 | 亮度锐度 | **色度锐度** |
|---|---|---|---|
| 修复前（近似最近邻 + 4:2:0） | 123 KB | 16.2 | 1.47 |
| **修复后（面积平均 + 4:4:4）** | 163 KB | **21.1**（+30%） | **3.07**（+109%） |
| 只修缩放，仍是 4:2:0 | 131 KB | 21.6 | 1.47 |
| 4:2:0 + 质量提到 90 | 205 KB | 21.1 | 1.46 |

**结论：糊来自缩放和色度采样，不是质量数字。** 把质量从 70 提到 90 对色度毫无帮助，只多花 25% 码率。

还能怎么再进一步：

| 想要 | 参数 | 代价（1080p60） |
|---|---|---|
| 原生分辨率，完全不做缩放 | `--height 1600` | 398 KB/帧 → 190 Mbps，管线 16.4 ms |
| 更省带宽（WiFi） | `--subsampling 420` | 163 → 131 KB/帧，色度锐度回到 1.47 |
| 更省 CPU | `--subsampling 420` | 编码 6.5 → 4.3 ms，帧率上限 80 → 94 fps |
| 更清楚但更重 | `--quality 85` | 163 → 215 KB/帧，锐度基本不再提升 |

## 特点

- **单个 exe，零 cargo 依赖**：没有 C 编译器、没有 .NET/Java 运行时、不需要 Visual Studio。
  `cargo build --release` 出来约 620 KB。
- **只发最新一帧**（MJPEG）：慢的观看端只会跳帧，不会越积越卡。
- **自适应码率**（MJPEG）：某个观看端发不出去时自动把 JPEG 质量从 70 降到最低 45。
- **鼠标是叠加层，不在视频里**：位置走 WebSocket，浏览器用 SVG 画。不占编码、缩放后依然清晰、
  也不会有"光标比画面慢"的拖影。
- **没人看就不干活**：MJPEG 模式停抓屏，H.264 模式直接杀掉 ffmpeg（编码器按需启停）。

## 需要准备什么

1. [Rust](https://rustup.rs/)，工具链固定为 **`stable-x86_64-pc-windows-msvc`**（见
   `rust-toolchain.toml`），因此需要 **Visual Studio 的「使用 C++ 的桌面开发」** 工作负载
   （或 Build Tools）。换成 MSVC 的原因：GNU 工具链在这台机器上缺 `dlltool`，任何需要
   proc-macro 的依赖都编不了；而且 MSVC 链接快 3 倍、产物小 35%：

   | 工具链 | 构建 | exe |
   |---|---|---|
   | GNU | 24.7 s | 754 KB |
   | **MSVC** | **7.5 s** | **489 KB** |
2. `turbojpeg.dll`：从 `lib/turbojpeg/x64/`（或 `x86/`）拿一份，放到 exe 旁边。
3. 只有 H.264 模式需要 `ffmpeg.exe`（**要带 NVENC**，官方默认构建没有）：

   ```powershell
   $ProgressPreference = 'SilentlyContinue'   # 不关进度条会慢一个数量级
   Invoke-WebRequest "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip" -OutFile ffmpeg.zip
   Expand-Archive ffmpeg.zip -DestinationPath .
   # 然后把 ffmpeg.exe 放到这个程序的旁边，或者用 --ffmpeg 指定路径
   ```

## 构建和运行

```powershell
cargo build --release
copy ..\OpenScreen\lib\turbojpeg\x64\turbojpeg.dll target\release\

.\target\release\openscreen-rs.exe info       # 抓屏尺寸、编码器状态
.\target\release\openscreen-rs.exe bench      # 量管线成本
.\target\release\openscreen-rs.exe selftest   # 校验 WebSocket 握手（SHA-1/base64 的 RFC 向量）
cargo test --release                          # 校验 H.264 码流切分

# MJPEG（零外部依赖）
.\target\release\openscreen-rs.exe serve --port 3032 --height 1080 --fps 60

# H.264（省 20 倍带宽）
.\target\release\openscreen-rs.exe serve --port 3033 --height 1080 --fps 60 `
    --h264 --bitrate 4000 --ffmpeg "D:\path\to\ffmpeg.exe"
```

然后在另一台电脑的浏览器里打开 `http://<这台机器的局域网IP>:3032/`（或 3033）。
**H.264 页面需要 Chrome/Edge 94+**（WebCodecs）。

参数：

| 参数 | 默认 | 说明 |
|---|---|---|
| `--port` | 3032 | 监听端口（`0.0.0.0`） |
| `--height` | 0（原生） | 帧高度；宽度按屏幕比例算，不会把 16:10 拉伸成 16:9 |
| `--fps` | 60 | 帧率上限 |
| `--quality` | 70 | MJPEG 质量（也是自适应码率的上限） |
| `--subsampling` | `444` | `444` 保留完整色彩细节；`420` 省 20% 带宽和 1/3 编码时间 |
| `--h264` | 关 | 用 H.264 + WebCodecs 播放页 |
| `--bitrate` | 4000 | H.264 码率（kbps） |
| `--mse` | 关 | H.264 用 MediaSource 播放，**`http://IP` 可直接看**（WebCodecs 则要求 localhost/HTTPS） |
| `--ffmpeg` | 自动找 | ffmpeg.exe 路径；依次找该参数、`OPENSCREEN_FFMPEG`、exe 同级目录、`PATH` |

### 已经修掉的坑（都有实测依据）

| 问题 | 症状 | 修法 |
|---|---|---|
| 主屏不能复制 | 选主屏黑屏/静止 | 抓屏阶梯：桌面复制 → GDI → 换一块能抓的屏 |
| GDI 坐标硬编码 | 多屏/缩放/4K 下 GDI 抓空 | 用 DXGI 报告的**该屏真实矩形**（位置+物理像素） |
| 掉线的观看端不回收 | 编码器为空会话空转 | WS 看门狗：10s 读超时 + ping，3 次无回应即断开 |
| 启动瞬间连入拿不到 offer | 页面卡在 connecting | Hub 在启动时创建；页面信令失败会重试 5 次 |
| 抓的屏和选的不一致 | 用户以为选错了 | `/settings` 返回 `capturing`，面板显示"实际抓取" |

### 依赖都在仓库里

```
vendor/ffmpeg/ffmpeg.exe      132 MB   LGPL 构建，带 nvenc/qsv/amf/MediaFoundation
vendor/ffmpeg/LICENSE.txt
vendor/turbojpeg/turbojpeg.dll  1.1 MB  MJPEG 编码
vendor/turbojpeg/LICENSE.md
```

打包脚本优先用这里的副本，所以**移走或删掉仓库外的原始下载也不会影响构建**。
只有第一次获取需要外部来源，之后整个仓库就是自包含的。

注意：这两个二进制体积较大（约 133 MB）。如果要提交到 git，历史会明显变大；
不提交的话它们仍会留在工作区里，误删的风险和普通源码文件一样。

### 打包

产物直接输出到 `target/build-release`（不打 zip）：

```powershell
powershell -ExecutionPolicy Bypass -File scripts\package.ps1
```

脚本会：静态链接 CRT 编译（不依赖 VC++ 运行库）→ 收集 ffmpeg / turbojpeg / 许可文件 →
复制 `scripts/release/` 里的启动脚本和说明 → 跑一次 `selftest` 自检 → 打印清单。

- `-SkipBuild` 跳过编译，只重新收集
- `-Ffmpeg <path>` / `-TurboJpeg <path>` 指定外部二进制位置（默认会在常见路径里找）
- `-Out <dir>` 换输出目录

启动脚本和用户说明放在 `scripts/release/`，改文案不用动脚本。

### 局域网用哪一个

| 想要 | 用什么 | 延迟（估算/实测） |
|---|---|---|
| **最低延迟** | **`--webrtc`** | **~30–60 ms**（RTP/UDP，无分片量化） |
| 兼容性最好 | `--mse` | ~90–110 ms（分片 16 ms + 缓冲 60→40 ms） |
| 零依赖、最稳 | 默认 MJPEG | ~40 ms，但走 TCP 且带宽 ~75 Mbps |
| 本机 localhost / HTTPS | `--h264`（WebCodecs） | ~50 ms |

`--mse` 走的是分片 MP4 + MediaSource：**不受安全上下文限制**，所以 `http://10.x.x.x:3033/` 直接能用。
代价是 MSE 会缓冲，比 WebCodecs 多约 200–400 ms。

**一个 Web 平台的坑**：Chrome 的自动播放策略可能让 `<video>` 完全不开始加载（`media.readyState`
永远是 `closed`），页面会在 2 秒后显示 "click to start"，**点一下就能播**。想免点击可以给浏览器加
`--autoplay-policy=no-user-gesture-required`，或在站点设置里允许自动播放。

### H.264 模式的清晰度

MJPEG 那条用自己写的面积平均缩放，H.264 那条由 ffmpeg 缩放，**算法选错就会明显发虚**：

| 缩放算法（同 4 Mbps） | 锐度 |
|---|---|
| `fast_bilinear`（曾经的默认） | 21.10 |
| `bicubic` | 21.06 |
| **`lanczos`（现在）** | **22.53** |

码率也有明显影响（同样 lanczos）：

| 码率 | 锐度 | 每帧 |
|---|---|---|
| 4 Mbps | 22.53 | 8 KB |
| **8 Mbps（现在默认）** | **23.43** | 17 KB |
| 12 Mbps | 23.49 | 24 KB |

也就是说：**lanczos + 8 Mbps 比原来的 bilinear + 4 Mbps 锐度高约 11%**，而 12 Mbps 基本没有额外收益。
顺带测了编码档位和码率控制方式（`p1` vs `p4`、`cbr` vs `vbr`+`cq`），差别都在 1% 以内，所以保
留对延迟最友好的 `p1 + cbr`。

### H.264 level 必须跟着分辨率走

H.264 的 level 是**宏块数和宏块率的硬上限**，超了编码器直接拒绝启动。`--height 1600` 时帧是
2560x1600 = 16000 宏块，而 level 4.2 只允许 8704，所以早先硬编码 `-level 4.2` 会一直报
"encoder produced nothing"。现在按帧尺寸和帧率自动选（4.2 / 5.0 / 5.1 / 5.2），**页面用的
codec 字符串也同步跟着变**（`avc1.64002A` → `avc1.640033`），否则 MediaSource 会拒绝这个流。

| 分辨率 @ 帧率 | level | codec 字符串 |
|---|---|---|
| 1728x1080 @ 60 | 4.2 | `avc1.64002A` |
| 2560x1600 @ 30 | 5.0 | `avc1.640032` |
| **2560x1600 @ 60** | **5.1** | **`avc1.640033`** |

实测 `--height 1600 --bitrate 8000` → **6.76 Mbps、2560x1600、codec avc1.640033**。

### 光标的两个易踩点

1. **画面里不能有光标**。ffmpeg 的 `ddagrab` 默认 `draw_mouse=1`，会把真实光标画进视频；而观看端
   还有一层光标叠加层，于是屏幕上出现两个光标，其中一个落后几百毫秒。所以抓屏一律用
   `draw_mouse=0`。验证方法：光标停在纯品红窗口上抓帧，数非背景像素——`draw_mouse=1` 是 77 个，
   `draw_mouse=0` 是 0 个。
2. **叠加层画的是系统原生光标，不是自己画的箭头**。服务端用 `GetCursorInfo` + `GetIconInfo` +
   `GetDIBits` 取到当前光标位图（32×32 RGBA）和热点，通过 WebSocket 发给页面；`/player.js` 把它
   画成 `background-image`，并按热点对齐。所以光标外观和你系统里的一模一样（包括自制光标）。
   形状只在切换时传一次（4 KB），位置每 10 ms 一个文本消息。
3. **叠加层的坐标要跟着视频的 letterbox 走**。`/player.js` 用 `min(框宽/视频宽, 框高/视频高)` 算出
   实际缩放再映射，所以窗口大小、缩放、黑边都不影响对位；三个播放页共用这一段代码。

### MSE 模式的延迟

MSE 天生比 WebCodecs 慢，因为它按分片播放。延迟由三部分构成，都能调：

| 组成 | 默认 | 调法 |
|---|---|---|
| 分片时长（一个分片到齐才出画面） | **`--fragment-ms 33` ≈ 31 ms** | 这是延迟地板，最有效的旋钮 |
| 页面允许的落后量 | 超过 120 ms 就微调播放速率追，超过 500 ms 才硬跳 | 页面里的 `catchUp()` |
| 解码与显示 | ~50–100 ms | 浏览器决定 |

**关键改动：分片按时长切，不再跟着关键帧走。** 以前分片长度 = GOP（6 帧 = 100 ms），而一个分片
必须整片到齐浏览器才能播，所以 100 ms 是硬地板。现在 `-frag_duration 33ms` 让 ffmpeg 按时间切
（实测 **p50 31 ms、p95 46 ms**，每片 64 KB），关键帧仍然单独起一片，所以：

* 新观看端 / 重置后**仍能从关键帧干净开始**（服务端会发 `{"keyframe":true}` 标记）；
* 但**分片不能再被丢弃**（时间切片不含关键帧，丢了就解不出来），落后时的处理是"清空缓冲 + 等下一个
  关键帧"，而不是跳片。

延迟结构因此从 ~300–350 ms 降到 **~150–200 ms**。`--gop` 的默认值也从 6 改回 60（现在它只决定
**恢复间隔**，也就是多久能重新同步一次，不再是延迟旋钮）。

页面上会实时显示 `live 1.2s, 0.08s behind, 57 fps, 340 fragments`：

* `behind` = 缓冲深度，延迟的主要可见部分
* `fps` = **实际送到屏幕的帧率**（`requestVideoFrameCallback` 统计），能区分"流慢"和"这台设备跟不上"
* 状态栏放在**左上角**，避免被 iPad/手机的底部栏挡住

## 端点

| 路径 | 内容 |
|---|---|
| `/` | 当前模式的播放页（MJPEG 或 H.264） |
| `/stream` | MJPEG 流（MJPEG 模式下任何其它路径也走这里） |
| `/h264` | H.264 播放页 |
| `/h264/ws` | WebSocket，二进制消息 = 一帧 H.264 访问单元（Annex-B），首条文本消息是解码器配置 |
| `/webrtc/ws` | WebSocket，WebRTC 信令（offer / answer） |
| `/ws` | WebSocket，推 `{"cursor":[x,y]}` 和 `{"control":布尔}`；开启远程控制后也接收 `m` / `b` / `w` / `k` / `r` 输入消息 |
| `/control` | 远程控制开关：`POST on=1` / `on=0`，`GET` 查当前状态（不重启编码器） |

## 验证脚本

```powershell
# MJPEG 的帧率/带宽
powershell -File scripts/measure.ps1 -Port 3032 -Seconds 6

# 帧的真实尺寸
powershell -File scripts/frame-info.ps1 -Port 3032

# H.264 的帧率/码率/编码器 CPU（WebCodecs 模式）
powershell -File scripts/measure-h264.ps1 -Port 3033 -Seconds 6

# MediaSource 模式的分片速率/码率
powershell -File scripts/measure-mse.ps1 -Port 3033 -Seconds 6

# 光标叠加层是否落在鼠标的位置上：摆一个品红窗口、把鼠标钉在中心、
# 用无头 Chrome 截图，再比较"窗口中心"和"画出来的箭头"
powershell -File scripts/cursor-check.ps1 -Port 3032

# 画质：先摆一张固定测试图，再抓一帧算锐度并导出一张 3 倍放大的裁切
powershell -STA -File scripts/test-pattern.ps1     # 另一个窗口里运行
powershell -File scripts/quality-check.ps1 -Port 3032 -Label mine
```

`cursor-check.ps1` 的期望输出：

```
magenta window:  (44,63) - (482,369)  centre (263,216)
cursor arrow:    (264,218) - (270,228)
offset:          dx=1 dy=2 px
```

`measure-h264.ps1` 的期望输出：

```
config:    {"config":{"codec":"avc1.64002A","width":1728,"height":1080,"fps":60}}
frames:    395 in 6.0s = 65.7 fps
bitrate:   3.33 Mbps, 6.2 KB per frame
encoder:   0.4% of the machine (8% of one core)
```

## 代码结构

```
src/main.rs       命令行入口：serve / bench / info / selftest
src/sys.rs        Win32 + COM 的最小 FFI（LoadLibraryW、按 vtable 槽位调方法、光标位置），
                  以及每个接口的槽位表——出错时先看这张表
src/capture.rs    DXGI 桌面复制：D3D11 设备 → 主输出 → AcquireNextFrame → staging 回读
src/scaler.rs     BGRA 盒式滤波缩放 + 保持比例的目标尺寸
src/jpeg.rs       libjpeg-turbo 运行时加载（tjInitCompress/tjCompress2）
src/mjpeg.rs      MJPEG 分帧（CRLF、boundary、禁缓存）
src/ws.rs         WebSocket 握手与文本/二进制帧（含手写 SHA-1 / base64，保持零依赖）
src/cursor.rs     读取系统原生光标位图（GetCursorInfo/GetIconInfo/GetDIBits）
src/input.rs      SendInput 注入：比例 → 绝对坐标、扫描码键盘、记住按下的键以便全部松开（含单元测试）
src/player_js.rs  /player.js：光标叠加层，三个播放页共用
src/page.rs       MJPEG 播放页（内嵌 HTML：`<img>` + SVG 光标叠加层 + 自动重连）
src/h264_page.rs  H.264 播放页（WebCodecs 解码 + 同一个光标叠加层）
src/h264.rs       H.264 码流切分：Annex-B → 访问单元（含单元测试）
src/fmp4.rs       分片 MP4 盒子切分：init 段 + 媒体分片（含单元测试）
src/mse_page.rs   MediaSource 播放页（http + IP 可用，含自动播放兜底）
src/webrtc_stream.rs  WebRTC 传输：每观看端一个 peer connection，访问单元经 RTP 发出
src/webrtc_page.rs    WebRTC 播放页（RTCPeerConnection + 状态栏显示 fps/rtt/jitter/丢包）
src/ffmpeg.rs     ffmpeg 子进程：ddagrab 抓屏 + NVENC 编码，raw Annex-B 出到 stdout
src/server.rs     单生产者 + 最新帧广播 + 自适应码率 + 光标广播 + 两种模式的路由
```

## 已知取舍

- **编码器起不来时**会打印 ffmpeg 自己的报错（例如 `Invalid Level`），并按 2/4/8/16 秒退避重试，
  不再每秒刷一行 "produced nothing"。

- **H.264 模式重启 ffmpeg 时**，正在看的页面会黑一下（等下一个关键帧，约 1 秒）。
- **H.264 不能像 MJPEG 那样丢帧**：帧间有依赖，所以慢的观看端只会被断开重连，而不是跳帧。
- ffmpeg 是 GPL 构建，**分发时要注意许可**；只在本机调用影响小，随产品打包需要重新评估。
- 带宽数字来自局域网/本机实测，公网还要看上行带宽和 RTT。
- **4:4:4 + 面积平均之后，1080p60 在本机只留了约 30% 的余量**（管线 12.5 ms / 16.7 ms 预算）。
  机器发烫降频时可能掉到 55 fps 左右；要余量就用 `--subsampling 420` 或 `--fps 50`。

## WebRTC 的可行性（已验证）

`smoothness` 的下一步只能是 UDP 传输。装好 VS 后验证过这条路的**基础是通的**：

| 验证项 | 结果 |
|---|---|
| `webrtc = "0.20"` 编译（含 ring 的 DTLS/SRTP 原生代码） | ✅ 40 s |
| proc-macro 依赖（serde derive） | ✅ 6 s |
| 运行期：建 peer connection | ✅ 10 ms |
| DTLS 证书 / 本地候选 / SDP 生成 | ✅ 10 个 host candidate，450 字节 SDP |
| 收尾 | ✅ 干净关闭 |

也就是说"零依赖"这个前提**已经放弃了**：换来的是可以走 WebRTC。还没做的是把它接进来
（视频轨 + SDP 走现有 HTTP 交换 + 页面换 `RTCPeerConnection`），预计 500–800 行。

## 还没做

- 远程控制通道的**口令**（现在只靠"默认关闭 + 可信局域网"）。
- 只抓某个窗口、系统声音。
- 更强的自适应：现在 H.264 用固定码率，可以按实际发送延迟动态调整。

## 许可

代码本身跟随原项目（MIT）。`turbojpeg.dll` 见 `../OpenScreen/lib/turbojpeg/LICENSE.md`；
ffmpeg 由用户自行下载，见其自身许可。
