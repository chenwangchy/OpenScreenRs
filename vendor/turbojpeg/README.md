# turbojpeg.dll

预编译的 [libjpeg-turbo](https://github.com/libjpeg-turbo/libjpeg-turbo) **3.1.1**（Windows，
MSVC 构建），从 NuGet 包 `turbojpeg-VC.Redist` 3.1.1 中取得，未做任何修改。

| 文件 | 架构 | SHA-256 |
|---|---|---|
| `x64/turbojpeg.dll` | x64 | `7396FCD2E27B26FEF8E175E8C35358063CACBB72B129BB04AE63ABFD0C3016CA` |
| `x86/turbojpeg.dll` | x86 | `84DCB0383CADF5075749F652283C2EBD72F08286879C2ABA73A1F5BAE63C9481` |

## 为什么放在这里

`OpenScreen.Core` 的 JPEG 编码优先走 libjpeg-turbo，实测 1728x1080 从 GDI+ 的约 8.3 ms
降到约 3 ms（同画质、更小体积）。**它不是硬依赖**：找不到 DLL 时 `JpegEncoder` 会自动
回退到 GDI+，功能不受影响，只是慢一些。

## 运行时放在哪

进程会在下面几处按顺序查找，命中即用：

1. `x64\turbojpeg.dll`（64 位进程）或 `x86\turbojpeg.dll`（32 位进程），相对 **exe 所在目录**；
2. 与 `OpenScreen.exe` 同目录的 `turbojpeg.dll`。

**注意：`OpenScreen.exe` 以 AnyCPU + Prefer32Bit 编译，在 64 位 Windows 上实际是 32 位进程**，
所以真正生效的是 `x86\turbojpeg.dll`。两个架构都带上最保险（换机器/换编译选项不用重发）。

`scripts/build.ps1` 打包时会自动把两个架构都放进输出目录；用 Visual Studio 构建时，
`OpenScreen.csproj` 里的 `<None ... CopyToOutputDirectory>` 项负责这一步。
**MSI 安装包（`.vdproj`）需要在 VS 里手动把 `x64`、`x86` 两个目录和许可文件加进去**，
否则装完只有 exe，程序会静默回退到 GDI+。


## 许可

libjpeg-turbo 使用 IJG 与 BSD 双许可，见同目录 `LICENSE.md`。分发本程序时需保留该许可文件。
