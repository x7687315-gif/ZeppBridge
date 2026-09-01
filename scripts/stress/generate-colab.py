#!/usr/bin/env python3
"""生成本地新增的测试文件到 Colab 云端沙盒笔记本。"""
import json
import os
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
COLAB_ROOT = "/content/ZeppBridge"

FILES = [
    ".github/workflows/stress-tests.yml",
    "docs/development/runtime-stress-tests.md",
    "package.json",
    "src-tauri/crates/core/tests/storage_stress.rs",
    "src-tauri/crates/core/tests/normalizer_decoder_stress.rs",
    "src-tauri/crates/core/tests/export_contract_stress.rs",
    "src-tauri/crates/core/tests/auth_stress.rs",
    "src-tauri/crates/cli/tests/runtime.rs",
    "src-tauri/crates/mcp/tests/runtime.rs",
    "tests/functions-stress.test.mjs",
]


def make_cell(cell_type, source):
    return {
        "cell_type": cell_type,
        "metadata": {},
        "source": source if isinstance(source, list) else source.splitlines(keepends=True),
    }


def main():
    cells = []
    cells.append(
        make_cell(
            "markdown",
            "# ZeppBridge 运行时强度测试 —— 云端沙盒\n\n"
            "本笔记本在 **Google Colab / Linux 云端** 运行，不会占用你的本地机器资源。\n\n"
            "流程：\n"
            "1. 安装 Rust + Node。\n"
            "2. 克隆 `lingcang728/ZeppBridge`。\n"
            "3. 写入全部新增/修改的强度测试文件。\n"
            "4. 运行 `cargo test`（core / cli / mcp）与 `npm run test:functions`。\n"
            "5. 输出结果。\n\n"
            "> 注意：压力级 `#[ignore]` 用例默认不跑；如需跑，把下面命令加上 `-- --ignored --nocapture`。",
        )
    )

    cells.append(
        make_cell(
            "code",
            "# 安装系统依赖\n"
            "!apt-get update -qq && apt-get install -y -qq libdbus-1-dev curl build-essential",
        )
    )

    cells.append(
        make_cell(
            "code",
            "# 安装 Rust（stable，带 clippy/rustfmt）\n"
            "!curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --component clippy,rustfmt\n"
            "import os\n"
            "os.environ['PATH'] += ':/root/.cargo/bin'",
        )
    )

    cells.append(
        make_cell(
            "code",
            "# 克隆仓库（只取最新提交，省流量）\n"
            "!rm -rf /content/ZeppBridge\n"
            "!git clone --depth 1 https://github.com/lingcang728/ZeppBridge.git /content/ZeppBridge",
        )
    )

    for rel in FILES:
        src = REPO_ROOT / rel
        content = src.read_text(encoding="utf-8")
        target = f"{COLAB_ROOT}/{rel}"
        # Colab %%writefile 会把 cell 内容写入文件；避免把 %%writefile 自身写进去
        code = f"%%writefile {target}\n{content}"
        cells.append(make_cell("code", code))

    cells.append(
        make_cell(
            "code",
            "# 安装 Node 20 与项目依赖\n"
            "!curl -fsSL https://deb.nodesource.com/setup_20.x | bash -\n"
            "!apt-get install -y nodejs\n"
            "%cd /content/ZeppBridge\n"
            "!npm ci",
        )
    )

    cells.append(
        make_cell(
            "code",
            "# 运行 Rust 日常级强度测试（core + cli + mcp）\n"
            "%cd /content/ZeppBridge\n"
            "!cargo test --manifest-path src-tauri/Cargo.toml -p zeppbridge-core -p zeppbridge-cli -p zeppbridge-mcp --locked -- --nocapture",
        )
    )

    cells.append(
        make_cell(
            "code",
            "# 运行 Cloudflare Functions 测试\n"
            "%cd /content/ZeppBridge\n"
            "!npm run test:functions",
        )
    )

    cells.append(
        make_cell(
            "code",
            "# （可选）压力级 Rust 用例：更长迭代 / 更高并发\n"
            "%cd /content/ZeppBridge\n"
            "!cargo test --manifest-path src-tauri/Cargo.toml -p zeppbridge-core --locked -- --ignored --nocapture",
        )
    )

    notebook = {
        "metadata": {
            "kernelspec": {
                "display_name": "Python 3",
                "language": "python",
                "name": "python3",
            },
            "language_info": {"name": "python"},
        },
        "nbformat": 4,
        "nbformat_minor": 5,
        "cells": cells,
    }

    out = REPO_ROOT / "scripts/stress/ZeppBridge_Runtime_Stress_Tests.ipynb"
    out.write_text(json.dumps(notebook, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"Generated {out}")


if __name__ == "__main__":
    main()
