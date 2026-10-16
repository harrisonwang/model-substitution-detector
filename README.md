# 模型照妖镜

大模型真伪检测工具，检测你使用的模型是否被冒充或调换。

macOS/Linux 安装：

```sh
brew install harrisonwang/tap/msd
```

Windows 10+（x64）安装：

从 [Releases](https://github.com/harrisonwang/model-substitution-detector/releases) 下载
`msd_<版本>_x86_64-pc-windows-msvc.zip`，解压后在 PowerShell 中进入 `msd` 目录：

```powershell
.\msd.exe -m gpt-6-astra
.\msd.exe -m gpt-6-astra -v
```

也可以将 `msd.exe` 所在目录加入用户 `PATH`，之后直接运行 `msd`。
首次 Windows 压缩包会随包含本次改动的版本发布。

需要已安装并登录 ChatGPT/Codex 桌面应用或 Codex CLI。Windows 会优先查找当前用户的
Microsoft Store 应用内置引擎、常见安装目录和 `%LOCALAPPDATA%\OpenAI\Codex` 下的引擎缓存，
随后查找 `PATH` 中的 `codex.exe` 或 npm 安装的 `@openai/codex` 原生可执行文件。
不需要 WSL、Git Bash 或单独安装 Node.js（使用桌面应用时）。仅安装不包含 Codex 引擎的
ChatGPT 客户端时，需要另装 Codex CLI。客户端自身的系统版本要求仍以其发行说明为准。

自定义桌面应用安装位置时，可以用 `MSD_APPLICATION_DIRS` 指定包含 `resources\codex.exe`
或 `app\resources\codex.exe` 的应用根目录；Windows 多个目录以分号分隔。
设置后会覆盖默认应用目录搜索，`PATH` 中的 CLI 仍会作为备用入口。
会话目录默认使用 `%USERPROFILE%\.codex`，也支持已有的 `CODEX_HOME` 设置。

运行：

```sh
msd -m gpt-6-astra
msd -m claude-fable-5.1（尚未实现）
msd --api https://openrouter.ai/api/v1 -m anthropic/claude-fable-5.1（尚未实现）
```

检测结果示例：

```text
gpt-6-astra ✓
gpt-6-astra ✗ → gpt-6-luna
gpt-6-astra ✗ → ?
gpt-6-astra ?
```

加 `-v` 查看检测详情：

```sh
msd -m gpt-6-astra -v
```

从源码构建：

```sh
cargo build --locked --release
./target/release/msd -m gpt-6-astra
```

Windows 源码构建需要 Rust MSVC 工具链（Rust 1.89+），以及 Visual Studio Build Tools 的
“使用 C++ 的桌面开发”组件和 Windows SDK：

```powershell
cargo build --locked --release
.\target\release\msd.exe -m gpt-6-astra
```

验证代码：

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

自动化测试使用本地原生模拟客户端，不会调用真实模型。CI 覆盖 macOS、Linux 和 Windows；
Windows 发布目标为 `x86_64-pc-windows-msvc`。

已安装客户端的 Windows 机器还可以运行以下检查，只验证入口、版本和采集所需功能，不发起模型请求：

```powershell
cargo test --locked --test windows installed_clients_support_collector_protocol -- --ignored --nocapture
```
