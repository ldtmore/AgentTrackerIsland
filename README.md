<div align="center">
  <img src="src-tauri/icons/icon.png" width="140" alt="去你的岛 AgentTrackerIsland 应用图标" />
</div>

<h1 align="center">去你的岛 · AgentTrackerIsland</h1> 

> **定位**：主流 Agent 状态实时监控灵动岛工作台 —— 旁路观测，只看，不碰。

[![release](https://img.shields.io/badge/release-v0.6.0-blue)](https://github.com/ldtmore/AgentTrackerIsland/releases)
[![tauri](https://img.shields.io/badge/Tauri-2-orange)](https://tauri.app)
[![rust](https://img.shields.io/badge/Rust-stable-success)](https://www.rust-lang.org)
[![license](https://img.shields.io/badge/license-MIT-green)](LICENSE)

## 这是什么

**去你的岛**（AgentTrackerIsland）是一个 Windows 优先的轻量桌面工具：以灵动岛形态实时监控各主流 AI Agent 的状态与消耗，有它，用户随时掌握各 Agent 的工作状态与 token/额度数据；没有它，各 Agent 照常工作——零影响。

- 🏝️ **灵动岛状态监控** — 颜色状态灯实时显示各 Agent（工作中/已完成/等待输入/出错），悬浮展开多 Agent 详情，贴边自动隐藏，点击跳转对应终端窗口
- 💰 **多供应商额度监控** — 供应商实例化管理，订阅窗口用量与按量余额实时显示，余额警戒线与窗口阈值告急直达岛/托盘
- 📊 **报表页** — 趋势堆叠图（四分类/按 Agent/按模型）、年度热力图、按模型/供应商聚合、CSV 导出
- 🗂️ **会话中心** — 独立会话窗口：分页/筛选/排序/关键字搜索、右侧详情抽屉（调用流水＋状态时间线）、CSV 所见即所得导出
- 🎛️ **设置页** — 分区卡片布局，设置项即时生效；深浅双主题（跟随系统/自选）、贴边隐藏、Agent 勾选与身份色、额度阈值均可视化配置

## 支持范围

- **Agent（16 家）**：Claude Code、Codex、Gemini CLI、Qwen Code、Kimi Code、OpenCode、Goose、Aider、GitHub Copilot CLI、Hermes Agent、OpenClaw、CodeBuddy、Qoder、WorkBuddy、MiMo Code、ZCode
- **供应商**：智谱 GLM（含 Z.ai）、DeepSeek、Moonshot Kimi（含国际站）、硅基流动（含国际站）、OpenRouter、Claude 订阅（本地推算）、自定义 OpenAI 兼容中转站

## 📥 下载安装

前往 [Releases](https://github.com/ldtmore/AgentTrackerIsland/releases) 下载最新的 `AgentTrackerIsland_x64-setup.exe`，双击按引导完成安装。

- 系统要求：Windows 10/11（x64）
- 默认为当前用户安装（免管理员权限），安装向导中可自行更改安装目录
- 安装包未做代码签名，首次运行如遇 Windows SmartScreen 提示，选择「更多信息 → 仍要运行」
- 升级方式：下载新版安装包直接覆盖安装，个人数据（数据库与设置）不受影响

## 设计哲学：观测台，不是网关

本项目是纯旁路观测工具，恪守五条设计红线：

1. **只看不碰** — 不代理、不改请求、不碰模型流量
2. **故障隔离** — 工具挂了/卸载了，Agent 照常工作，零感知
3. **顺序无关** — 后开工具也能回溯补录历史数据
4. **渐进降级** — hooks → 文件监听 → 进程监控，层层兜底
5. **不抢焦点** — 默认静默，打扰一律 opt-in

## 技术栈

Tauri 2 · Rust · React + TypeScript · SQLite（rusqlite）· ECharts

## License

本项目基于 [MIT License](LICENSE) 协议开源。

Copyright (c) 2026 LDT · Made with AI
