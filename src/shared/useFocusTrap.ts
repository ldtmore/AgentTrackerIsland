/**
 * 模态焦点管理 hook（2026-10-03 审查新增，对标 WAI-APG 对话框模式/macOS Sheet）：
 * 打开时初始聚焦首个可聚焦元素、Tab 在容器首尾循环（焦点陷阱）、关闭时焦点
 * 归还触发元素。此前设置弹窗/会话抽屉只有 Esc＋遮罩，aria-modal 是虚假声明
 * ——键盘/读屏用户会 Tab 穿透到遮罩下的整页内容。
 *
 * 用法：const ref = useRef<HTMLDivElement>(null); useFocusTrap(ref, open);
 * ref 挂在模态容器（弹窗/抽屉根元素）上；open 由 false→true 时聚焦，
 * true→false 时归焦
 */
import { useEffect } from "react";

/** 可聚焦元素选择器（排除禁用与负 tabIndex） */
const FOCUSABLE =
  'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [href], [tabindex]:not([tabindex="-1"])';

export function useFocusTrap(
  ref: React.RefObject<HTMLElement | null>,
  active: boolean,
): void {
  useEffect(() => {
    if (!active) return;
    const root = ref.current;
    if (!root) return;
    // 记录打开前的焦点（关闭时归还）
    const prevActive = document.activeElement as HTMLElement | null;

    // 初始聚焦：优先首个表单控件（用户最常见的首个动作），否则首个可聚焦元素
    const first =
      root.querySelector<HTMLElement>("input, select, textarea") ??
      root.querySelector<HTMLElement>(FOCUSABLE);
    first?.focus();

    // Tab 循环：容器首尾环绕（Shift+Tab 反向）。监听挂 document 捕获阶段
    // （2026-10-17 审查 6.1）：挂 root 上的 keydown 只在焦点路径经过 root 时
    // 触发——点击弹窗内非可聚焦区域（标题/hint）后焦点被收拢到 body，事件
    // 从 body 派发不再经过 root，Tab 直达遮罩背后的页面（aria-modal 失效）。
    // 捕获阶段兜住全部按键；焦点在容器外时一律拉回（下方 !root.contains 分支）
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key !== "Tab") return;
      const items = Array.from(root.querySelectorAll<HTMLElement>(FOCUSABLE)).filter(
        (el) => el.offsetParent !== null || el === document.activeElement,
      );
      if (items.length === 0) return;
      const firstEl = items[0];
      const lastEl = items[items.length - 1];
      const cur = document.activeElement;
      if (e.shiftKey) {
        if (cur === firstEl || !root.contains(cur)) {
          e.preventDefault();
          lastEl.focus();
        }
      } else {
        if (cur === lastEl || !root.contains(cur)) {
          e.preventDefault();
          firstEl.focus();
        }
      }
    };
    document.addEventListener("keydown", onKeyDown, true);
    return () => {
      // 移除必须带同款捕获标记，否则监听摘不掉
      document.removeEventListener("keydown", onKeyDown, true);
      // 关闭归焦（元素仍在文档中才有效；卸载场景静默失败可接受）
      if (prevActive && document.contains(prevActive)) prevActive.focus();
    };
  }, [active, ref]);
}
