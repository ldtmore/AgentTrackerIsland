# LiteLLM 单价表快照 revision 记录

本目录的 `model_prices_and_context_window.json` 是
[BerriAI/litellm](https://github.com/BerriAI/litellm) 单价表的**锁定快照**
（MIT 许可），随应用编译进二进制（`include_str!`），离线可用。
更新流程见下方；上游每天多次更新（全走 PR），本项目按需手动更新（YAGNI，不做自动工作流）。

## 当前快照

| 项 | 值 |
|----|----|
| 上游 revision | `ddc7ee6838726f2c329202bc4bef9abca58e1b35`（main 分支） |
| 下载日期 | 2026-09-24 |
| 条目数 | 4328 |
| 下载地址 | `https://raw.githubusercontent.com/BerriAI/litellm/ddc7ee6838726f2c329202bc4bef9abca58e1b35/model_prices_and_context_window.json` |
| 消费方 | `src-tauri/src/modelcat.rs`（`PRICES_REVISION` 常量与此文件须同步更新） |

## 更新流程（手动）

1. 取上游最新 commit：`https://api.github.com/repos/BerriAI/litellm/commits/main` 的 `sha` 字段；
2. 按上面下载地址的模式（sha 替换）下载新 JSON，覆盖本目录同名文件；
3. 更新本文件的 revision 表与 `modelcat.rs` 中的 `PRICES_REVISION` 常量；
4. 跑 `cargo test`（含快照抽验单测：条目数下限、已知模型字段非零）确认解析正常。
