/**
 * 可搜索下拉（M1-10 从报表页抽出共享）：筛选项多时输入关键字模糊匹配快速定位；
 * 报表页与会话窗口共用，样式随组件自带（searchselect.css）。
 * value 为过滤原值（项目为全路径），label 为显示文本（项目为尾段）
 */
import { useEffect, useRef, useState } from "react";

export default function SearchSelect({
  value,
  options,
  allLabel,
  onChange,
}: {
  /** 当前选中值；null=全部 */
  value: string | null;
  options: { value: string; label: string }[];
  allLabel: string;
  onChange: (v: string | null) => void;
}) {
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
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

  return (
    <div className="sel" ref={ref}>
      <button
        type="button"
        className={`sel-btn${open ? " open" : ""}`}
        onClick={() => {
          setOpen(!open);
          setQ(""); // 每次打开重置搜索词
        }}
      >
        <span className="sel-text" title={current?.value ?? allLabel}>
          {current ? current.label : allLabel}
        </span>
        <span className="sel-caret">▾</span>
      </button>
      {open && (
        <div className="sel-pop">
          <input
            className="sel-search"
            autoFocus
            value={q}
            placeholder="输入关键字过滤…"
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setOpen(false);
              if (e.key === "Enter" && filtered.length > 0) {
                onChange(filtered[0].value);
                setOpen(false);
              }
            }}
          />
          <div className="sel-list">
            <div
              className={`sel-opt${value == null ? " on" : ""}`}
              onClick={() => {
                onChange(null);
                setOpen(false);
              }}
            >
              {allLabel}
            </div>
            {filtered.map((o) => (
              <div
                key={o.value}
                className={`sel-opt${value === o.value ? " on" : ""}`}
                title={o.value}
                onClick={() => {
                  onChange(o.value);
                  setOpen(false);
                }}
              >
                {o.label}
              </div>
            ))}
            {filtered.length === 0 && <div className="sel-none">无匹配项</div>}
          </div>
        </div>
      )}
    </div>
  );
}
