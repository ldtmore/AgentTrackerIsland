/**
 * 操作结果反馈 toast（2026-10-09 自设置页 .st-toast 提炼共享，对齐主流通行做法）：
 * 设置页保存反馈与报表/会话页导出反馈同一套语言——成功短暂自动消失（时长可配，
 * 缺省 2500＝设置页保存反馈口径），失败常驻可关闭；读屏语义 status/alert 分级
 * （2026-10-03 审查口径随组件走）。
 * 定时器归组件自管：调用方 showToast 只摆数据，不再各自养定时器；
 * 定位固定贴窗顶居中（AntD 同位，落在页头中部空区不盖按钮），让位需求由
 * 调用方追加定位类（设置页 st-toast-top 让位吸顶锚点条）。
 */
import { useCallback, useEffect, useRef } from "react";
import { CloseIcon } from "./icons";
import "./toast.css";

/** 单条 toast 的数据（null＝不显示）；调用方以 state 持有 */
export interface ToastData {
  text: string;
  kind: "ok" | "error";
  /** 动作链接（如导出成功的「打开所在目录」）：点击执行并收起 toast */
  action?: { label: string; onClick: () => void };
  /** 成功态自动消失毫秒数；失败恒常驻（与设置页「失败常驻待处理」纪律一致） */
  durationMs?: number;
  /** 悬停暂停自动消失倒计时（带动作的成功反馈用，VS Code 通知同款细节） */
  pauseOnHover?: boolean;
}

export default function Toast({
  data,
  onDismiss,
  className,
}: {
  data: ToastData | null;
  onDismiss: () => void;
  /** 追加定位类（与共享样式解耦：如设置页传 st-toast-top） */
  className?: string;
}) {
  // onDismiss 走 ref：调用方传内联箭头（每次渲染新引用）也不重置自动消失计时
  const dismissRef = useRef(onDismiss);
  dismissRef.current = onDismiss;
  const timerRef = useRef<number | null>(null);
  // 成功态时长（失败恒 null＝常驻）；arm 供入场与悬停移出恢复共用
  const duration = data && data.kind === "ok" ? data.durationMs ?? 2500 : null;
  const arm = useCallback(() => {
    if (timerRef.current !== null) window.clearTimeout(timerRef.current);
    timerRef.current =
      duration != null
        ? window.setTimeout(() => dismissRef.current(), duration)
        : null;
  }, [duration]);
  useEffect(() => {
    if (!data) return;
    arm();
    return () => {
      if (timerRef.current !== null) window.clearTimeout(timerRef.current);
    };
  }, [data, arm]);
  if (!data) return null;
  const { text, kind, action } = data;
  // 悬停暂停：清计时；移出重新计满（正在读/要去点的用户不被倒计时打断）
  const pausable = data.pauseOnHover === true && duration != null;
  const pause = () => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current);
      timerRef.current = null;
    }
  };
  return (
    <div
      className={`toast${kind === "error" ? " toast-err" : ""}${className ? ` ${className}` : ""}`}
      /* 错误用 assertive 主动播报：操作失败读屏用户需要立即感知（2026-10-03 口径） */
      role={kind === "error" ? "alert" : "status"}
      onMouseEnter={pausable ? pause : undefined}
      onMouseLeave={pausable ? arm : undefined}
    >
      <span className="toast-text">{text}</span>
      {action && (
        <button
          type="button"
          className="toast-action"
          onClick={() => {
            action.onClick();
            dismissRef.current();
          }}
        >
          {action.label}
        </button>
      )}
      {/* 失败常驻但可主动关掉（五轮审查口径随组件迁移）：错误串可能很长，
          一直压着页面又无关闭手段＝只能等下一次操作覆盖 */}
      {kind === "error" && (
        <button
          type="button"
          className="toast-close"
          aria-label="关闭提示"
          onClick={() => dismissRef.current()}
        >
          <CloseIcon />
        </button>
      )}
    </div>
  );
}
