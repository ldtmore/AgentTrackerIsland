/**
 * 关于页（关于窗口）：项目简介 + 版本号 + GitHub 仓库链接
 * - 定位展示：旁路观测工具「只看不碰」的一句话定位，帮助用户理解工具边界
 * - 版本号：getVersion() 从 Rust 侧（tauri.conf.json）读取，前端不硬编码
 * - 升级策略（所有者拍板，程序简单化）：程序内不做任何检测/下载/升级，
 *   仅提供仓库链接与小字引导，由用户自行跳转浏览器下载安装包手动升级
 * - 交互：点链接经 Rust open_repository 打开系统默认浏览器；「复制」走
 *   navigator.clipboard，不可用时提示手动选择复制
 * - 样式：ab-* 前缀，色板全走 App.css 主题变量，与设置页/报表页同规范
 */
import { useEffect, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { invoke } from "@tauri-apps/api/core";
import { useTheme } from "../shared/theme";
import "./about.css";

// 应用图标（构建期由 Vite 打包；128px 源图缩小展示，高分屏下仍清晰）
import appIcon from "../../src-tauri/icons/128x128.png";

/** 仓库地址展示/复制文案（与 Rust 侧 open_repository 的 REPO_URL 常量保持同步，两处同改） */
const REPO_URL = "https://github.com/ldtmore/AgentTrackerIsland";

export default function About() {
  // 主题应用与跟随（各窗口入口调一次）
  useTheme();
  // 当前版本号（读失败显示占位，不打扰）
  const [version, setVersion] = useState("…");
  // 复制成功的短暂反馈（2s 后还原按钮文案）
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    getVersion()
      .then(setVersion)
      .catch(() => setVersion("未知"));
  }, []);

  /** 在系统默认浏览器打开仓库页（URL 由 Rust 侧常量持有，前端不传参） */
  const openRepo = () => {
    invoke("open_repository").catch(() => {});
  };

  /** 复制仓库链接；剪贴板不可用时提示手动选择复制（不弹错误打扰） */
  const copyRepo = async () => {
    try {
      await navigator.clipboard.writeText(REPO_URL);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 2000);
    } catch {
      window.alert("复制失败，请手动选择链接文本复制");
    }
  };

  return (
    <div className="ab-root">
      {/* 品牌区：图标 + 中英文名 + 口号 + 一句话定位 */}
      <section className="ab-hero">
        <img className="ab-logo" src={appIcon} alt="去你的岛应用图标" />
        <div className="ab-names">
          <span className="ab-name-zh">去你的岛</span>
          <span className="ab-name-en">AgentTrackerIsland</span>
        </div>
        <div className="ab-slogan">主流 Agent 状态实时监控灵动岛工作台</div>
        <p className="ab-intro">
          去你的岛（AgentTrackerIsland），是一款主流 AI Agent 的
          旁路观测工具：只看不碰。实时掌握各 Agent 的运行状态与用量消耗；
          有它无它，你的原有工作流照常运转，零影响。
        </p>
      </section>

      {/* 版本与仓库：版本号只读展示；升级交由用户手动完成，程序内不自动处理 */}
      <section className="ab-section">
        <div className="ab-row">
          <div className="ab-row-main">
            <div className="ab-row-text">
              <div className="ab-row-title">当前版本</div>
              <div className="ab-row-desc">版本号随应用发布，由安装包一并更新</div>
            </div>
            <div className="ab-row-control">
              <span className="ab-version">{version}</span>
            </div>
          </div>
        </div>
        <div className="ab-row">
          <div className="ab-row-main">
            <div className="ab-row-text">
              <div className="ab-row-title">GitHub 仓库</div>
              <button type="button" className="ab-link" onClick={openRepo}>
                {REPO_URL}
              </button>
              <div className="ab-hint">
                点击上方链接（或复制到浏览器）打开项目仓库下载安装
              </div>
            </div>
            <div className="ab-row-control">
              <button type="button" className="ab-btn" onClick={copyRepo}>
                {copied ? "已复制" : "复制"}
              </button>
            </div>
          </div>
        </div>
      </section>

      {/* 页脚：红线关键词 + 版权署名（作者 LDT，AI 协助开发） */}
      <div className="ab-foot">
        <div>旁路观测 · 只看不碰 · 故障隔离</div>
        <div>© 2026 LDT · Made with AI</div>
      </div>
    </div>
  );
}
