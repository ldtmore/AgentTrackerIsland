/**
 * 全窗加载指示（懒创建业务窗长空窗治理）：Suspense fallback（chunk 加载期）
 * 与 main.tsx 就绪门等待期共用一套视觉。首帧 100~300ms 短过渡维持 index.html
 * 二稿定稿的纯底色不加内容——本组件只出现在可感知的长空窗，把「死寂的空白」
 * 变成「可知的等待」。样式在 App.css 的 .boot-loading 系列（就绪门提示为纯
 * DOM 注入，同样消费该类名）；呼吸动画受全局 reduced-motion 收敛自动退化。
 */
export function BootLoading({ text = "正在加载…" }: { text?: string }) {
  return (
    <div className="boot-loading" role="status" aria-live="polite">
      <img src="/logo.svg" alt="" className="boot-loading-logo" />
      <span className="boot-loading-text">{text}</span>
    </div>
  );
}
