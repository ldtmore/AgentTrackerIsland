<div align="center">
  <img src="src-tauri/icons/icon.png" width="140" alt="去你的岛 AgentTrackerIsland 应用图标" />
</div>

<h1 align="center">去你的岛 · AgentTrackerIsland</h1> 

> **定位**：主流 Agent 状态实时监控灵动岛工作台 —— 旁路观测，只看，不碰。

[![status](https://img.shields.io/badge/status-M1_进行中（报表·双主题·贴边隐藏落地）-blue)](docs/03-TASKS.md)
[![tauri](https://img.shields.io/badge/Tauri-2-orange)](https://tauri.app)
[![rust](https://img.shields.io/badge/Rust-stable-success)](https://www.rust-lang.org)

## 这是什么

**去你的岛**（AgentTrackerIsland）是一个 Windows 优先的轻量桌面工具：以灵动岛形态实时监控各主流 AI Agent 的状态与消耗，有它，用户随时掌握各 Agent 的工作状态与 token/额度数据；没有它，各 Agent 照常工作——零影响。

- 🏝️ **灵动岛状态监控** — 颜色状态灯实时显示各 Agent（工作中/已完成/等待输入/出错），悬浮展开多 Agent 详情，贴边自动隐藏，点击跳转对应终端窗口
- 💰 **用量与额度统计** — token 消耗按供应商/模型/时间统计；订阅额度（如 GLM Coding Plan 的 5 小时/每周窗口）实时显示用量百分比与重置倒计时
- 📊 **报表页** — 趋势图、热力图、按模型/供应商聚合
- 🎛️ **设置页** — 分区卡片布局，设置项即时生效；深浅双主题（跟随系统/自选）、贴边隐藏、Agent 勾选与身份色、额度阈值均可视化配置

## 设计哲学：观测台，不是网关

本项目是纯旁路观测工具，遵守 [开发宪法](AGENTS.md) 五条红线：

1. **只看不碰** — 不代理、不改请求、不碰模型流量
2. **故障隔离** — 工具挂了/卸载了，Agent 照常工作，零感知
3. **顺序无关** — 后开工具也能回溯补录历史数据
4. **渐进降级** — hooks → 文件监听 → 进程监控，层层兜底
5. **不抢焦点** — 默认静默，打扰一律 opt-in

## 技术栈

Tauri 2 · Rust · React + TypeScript · SQLite（rusqlite）· ECharts

## 文档

- [开发宪法](AGENTS.md) — 定位与红线，贡献前必读
- [设计总览](docs/02-DESIGN.md) — 架构、模块设计与审查回写
- [任务清单](docs/03-TASKS.md) — T0–T13 与 M1 任务明细
- [交接快照](docs/HANDOFF.md) — 会话进度与待办（接手必读）
- [协作工作流](docs/WORKFLOW.md) — 三阶段流程与多 Agent 交接规则
- [项目计划 v1（草案归档）](docs/PLAN.md) — 早期 M0 设计，阶段 2 将产出正式方案取代

## License

未定（开源发布前确定）
