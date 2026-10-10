/**
 * 真空态统一模板（07-UX 2.1）：大号弱色线性图标（可选）＋标题＋一句说明＋
 * 主按钮（可选）。以额度页 `.qt-empty` 的既有结构为准（标题＋说明＋主按钮），
 * 报表／会话页原先的一行灰字升级为同一模板；筛选空态（用户任务中途）不归
 * 此管，各页保持轻量文案。图标复用 shared/icons 的线性语义图标，经 CSS
 * 放大弱化（见 emptystate.css 的描边换算注释）。
 */
import "./emptystate.css";

export default function EmptyState({
  icon,
  title,
  desc,
  action,
}: {
  /** 大号线性图标（icons.tsx 语义图标：报表 ReportIcon／会话 SessionsIcon／
   *  额度 CoinIcon／错误 WarnIcon 各归其位） */
  icon?: React.ReactNode;
  /** 主标题：一句话说清「这里为什么是空的」 */
  title: string;
  /** 补充说明（数据何时出现／下一步怎么办），可省 */
  desc?: React.ReactNode;
  /** 主按钮（label＋onClick）：有明确动作才给，真空态无动作不造按钮 */
  action?: { label: string; onClick: () => void };
}) {
  return (
    <div className="es-wrap">
      {icon && <span className="es-icon">{icon}</span>}
      <div className="es-title">{title}</div>
      {desc && <div className="es-desc">{desc}</div>}
      {action && (
        <button type="button" className="es-btn" onClick={action.onClick}>
          {action.label}
        </button>
      )}
    </div>
  );
}
