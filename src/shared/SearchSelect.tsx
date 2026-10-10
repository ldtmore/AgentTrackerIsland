/**
 * 可搜索下拉（M1-10 从报表页抽出共享）：筛选项多时输入关键字模糊匹配快速定位；
 * 报表页与会话窗口共用，样式随组件自带（searchselect.css）。
 * value 为过滤原值（项目为全路径），label 为显示文本（项目为尾段）。
 * 2026-09-29 审查修复 #9b：新增 multi 多选模式（报表页对比筛选用）——
 * 选中集合即时 toggle 不关浮层，「全部」清空并关闭；单选行为保持不变（会话窗口零改动）。
 * 2026-10-03 审查补充：listbox 语义＋方向键导航（↑↓ 移动高亮、Enter 选中高亮项、
 * Esc 关闭）——此前只有鼠标可操作选项行
 */
import { useEffect, useRef, useState } from "react";
import { ChevronIcon } from "./icons";
import "./searchselect.css";

export default function SearchSelect({
  value,
  values,
  options,
  allLabel,
  onChange,
  onValues,
  multi = false,
}: {
  /** 单选：当前选中值；null=全部（multi 模式不使用） */
  value?: string | null;
  /** 多选：当前选中集合；null=全部（非 multi 模式不使用） */
  values?: string[] | null;
  options: { value: string; label: string }[];
  allLabel: string;
  /** 单选回调 */
  onChange?: (v: string | null) => void;
  /** 多选回调（「全部」→null；清单非空但为 [] 不会出现——再点即移除最后一项前先归 null） */
  onValues?: (v: string[] | null) => void;
  /** 多选模式开关 */
  multi?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  /** 键盘高亮行（0=「全部」，1..n=filtered 项；-1=无高亮） */
  const [hi, setHi] = useState(-1);
  const ref = useRef<HTMLDivElement>(null);

  // 点击组件外部关闭浮层
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  // 模糊过滤：label 与 value 都参与匹配（项目可按全路径关键字搜）
  const kw = q.trim().toLowerCase();
  const filtered = kw
    ? options.filter(
        (o) => o.label.toLowerCase().includes(kw) || o.value.toLowerCase().includes(kw),
      )
    : options;
  const current = options.find((o) => o.value === value);
  // 多选按钮文案：全部 → 单项 label → 「A、B 等 n 项」
  const multiLabel =
    values == null || values.length === 0
      ? allLabel
      : values.length === 1
        ? (options.find((o) => o.value === values[0])?.label ?? values[0])
        : `${options.find((o) => o.value === values[0])?.label ?? values[0]} 等 ${values.length} 项`;

  /** 多选 toggle（浮层保持打开；清到空集合归 null=全部） */
  const toggleMulti = (v: string) => {
    const next =
      values == null
        ? [v]
        : values.includes(v)
          ? values.length === 1
            ? null
            : values.filter((x) => x !== v)
          : [...values, v];
    onValues?.(next);
  };

  /** 选中第 idx 行（0=「全部」）：多选 toggle 不关浮层，单选/全部即关 */
  const pick = (idx: number) => {
    if (idx <= 0) {
      if (multi) onValues?.(null);
      else onChange?.(null);
      setOpen(false);
      return;
    }
    const o = filtered[idx - 1];
    if (!o) return;
    if (multi) {
      toggleMulti(o.value);
    } else {
      onChange?.(o.value);
      setOpen(false);
    }
  };

  /** 高亮行数（含「全部」行）；方向键环形移动 */
  const rowCount = filtered.length + 1;
  const moveHi = (dir: 1 | -1) =>
    setHi((h) => (h < 0 ? (dir === 1 ? 0 : rowCount - 1) : (h + dir + rowCount) % rowCount));

  return (
    <div className="sel" ref={ref}>
      <button
        type="button"
        className={`sel-btn${open ? " open" : ""}`}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => {
          setOpen(!open);
          setQ(""); // 每次打开重置搜索词
          setHi(-1);
        }}
      >
        <span className="sel-text" title={multi ? multiLabel : (current?.value ?? allLabel)}>
          {multi ? multiLabel : current ? current.label : allLabel}
        </span>
        <span className="sel-caret"><ChevronIcon dir="down" /></span>
      </button>
      {open && (
        <div className="sel-pop">
          <input
            className="sel-search"
            autoFocus
            value={q}
            placeholder="输入关键字过滤…"
            onChange={(e) => {
              setQ(e.target.value);
              setHi(-1); // 过滤变化重置高亮
            }}
            onKeyDown={(e) => {
              if (e.key === "Escape") setOpen(false);
              if (e.key === "ArrowDown") {
                e.preventDefault();
                moveHi(1);
              }
              if (e.key === "ArrowUp") {
                e.preventDefault();
                moveHi(-1);
              }
              if (e.key === "Enter") {
                // 高亮项优先；无高亮维持旧行为（首项）
                if (hi >= 0) pick(hi);
                else if (filtered.length > 0) pick(1);
              }
            }}
          />
          <div className="sel-list" role="listbox">
            <div
              className={`sel-opt${(multi ? values == null : value == null) ? " on" : ""}${hi === 0 ? " hi" : ""}`}
              role="option"
              aria-selected={multi ? values == null : value == null}
              onMouseEnter={() => setHi(0)}
              onClick={() => pick(0)}
            >
              {allLabel}
            </div>
            {filtered.map((o, i) => {
              const on = multi ? (values?.includes(o.value) ?? false) : value === o.value;
              return (
                <div
                  key={o.value}
                  className={`sel-opt${on ? " on" : ""}${hi === i + 1 ? " hi" : ""}`}
                  role="option"
                  aria-selected={on}
                  title={o.value}
                  onMouseEnter={() => setHi(i + 1)}
                  onClick={() => pick(i + 1)}
                >
                  {multi && <span className="sel-check">{on ? "✓" : ""}</span>}
                  {o.label}
                </div>
              );
            })}
            {filtered.length === 0 && <div className="sel-none">无匹配项</div>}
          </div>
        </div>
      )}
    </div>
  );
}
