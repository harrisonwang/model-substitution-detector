# 模型照妖镜

大模型真伪检测工具，检测你使用的模型是否被冒充或调换。

macOS/Linux 安装：

```sh
brew install harrisonwang/tap/msd
```

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
