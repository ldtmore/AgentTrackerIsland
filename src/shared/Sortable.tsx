/**
 * 卡片拖拽排序封装（2026-10-03 设置页排序，@dnd-kit）：
 * - SortableList：DndContext＋SortableContext 容器（网格排序策略）。分组
 *   形态可嵌套多个（组头一个、每组卡片区各一个）——各自独立的 DndContext
 *   让「跨组拖拽」在结构上就不可能发生，无需 id 前缀路由；
 * - SortableItem：卡片单元格。grip 手柄悬浮时浮现于卡片顶边，指针拖拽
 *   （5px 位移阈值与点击区分）与键盘排序（dnd-kit 内建：聚焦手柄后空格
 *   拾起、方向键移动、空格放下，aria 操作说明自动注入）共用同一手柄；
 * - SortableBox：整块排序单元（分组形态的组）。children 为函数，把手柄
 *   props 摊到调用方自带的头行按钮上——点击折叠照常，按住拖动＝整组平移。
 * 选型依据：原生 HTML5 拖放在 Tauri 2 Windows 下被默认 dragDropEnabled
 * 拦截，dnd-kit 的 Pointer 传感器不受影响；卡片上的按钮/开关不受手柄
 * 干扰（手柄是卡片段之外的兄弟节点，且位移阈值兜底）。
 * 落库纪律归调用方：拖完即存即生效、失败回滚＋toast（与勾选/启停同款）。
 */
import type { DragEndEvent, DraggableAttributes } from "@dnd-kit/core";
import {
  DndContext,
  KeyboardCode,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  rectSortingStrategy,
  sortableKeyboardCoordinates,
  useSortable,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { GripIcon } from "./icons";

/** 列表内重排助手：active 移到 over 的位置；任一 id 不在列表（或原地放下）
 *  返回 null，调用方据此静默丢弃 */
export function moveIn(ids: string[], activeId: string, overId: string): string[] | null {
  const from = ids.indexOf(activeId);
  const to = ids.indexOf(overId);
  return from >= 0 && to >= 0 && from !== to ? arrayMove(ids, from, to) : null;
}

/** 拖拽排序容器：onDragEnd 收原始事件，调用方用 moveIn 计算新序并持久化 */
export function SortableList({
  ids,
  onDragEnd,
  children,
}: {
  /** 参与排序的条目 id（顺序＝当前展示序） */
  ids: string[];
  onDragEnd: (e: DragEndEvent) => void;
  children: React.ReactNode;
}) {
  // 位移阈值 5px：手柄/头行同时承担点击职责时不误触发拖拽。
  // 键盘拾起键收窄为仅空格（默认含 Enter 且会 preventDefault）：组头这类
  // 「点击＋拖拽」双职责按钮上 Enter 必须留给原生点击（折叠/展开），
  // 空格拾起——与 dnd 屏幕阅读器指引文案「press the space bar」一致
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 5 } }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
      keyboardCodes: {
        start: [KeyboardCode.Space],
        cancel: [KeyboardCode.Esc],
        end: [KeyboardCode.Space, KeyboardCode.Tab],
      },
    }),
  );
  return (
    <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
      <SortableContext items={ids} strategy={rectSortingStrategy}>
        {children}
      </SortableContext>
    </DndContext>
  );
}

/** 可排序卡片单元格：children＝卡片段（.st-agent／ProviderCard 等）。
 *  拖动中 transform 跟随指针，其余单元让位过渡（reduced-motion 下过渡
 *  由全局媒体查询关闭）；.sort-grip 定位样式见 settings.css */
export function SortableItem({
  id,
  children,
}: {
  id: string;
  children: React.ReactNode;
}) {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } =
    useSortable({ id });
  return (
    <div
      ref={setNodeRef}
      className={`sort-cell${isDragging ? " sort-dragging" : ""}`}
      style={{ transform: CSS.Translate.toString(transform), transition }}
    >
      <button
        type="button"
        ref={setActivatorNodeRef}
        className="sort-grip"
        aria-label="拖动调整顺序（聚焦后也可用方向键移动）"
        {...attributes}
        {...listeners}
      >
        <GripIcon />
      </button>
      {children}
    </div>
  );
}

/** 整块排序单元（分组形态的组头）：children 为函数，把手柄 props 摊到
 *  调用方的头行按钮上（ref＋attributes＋listeners 合并为 handleProps） */
export function SortableBox({
  id,
  children,
}: {
  id: string;
  children: (handle: {
    /** 拖拽激活器 ref（挂在头行按钮上） */
    ref: (el: HTMLButtonElement | null) => void;
    /** dnd 的 aria 属性＋指针监听（摊到头行按钮） */
    handleProps: DraggableAttributes & Record<string, unknown>;
  }) => React.ReactNode;
}) {
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, transform, transition, isDragging } =
    useSortable({ id });
  return (
    <div
      ref={setNodeRef}
      className={`sort-box${isDragging ? " sort-dragging" : ""}`}
      style={{ transform: CSS.Translate.toString(transform), transition }}
    >
      {children({
        ref: setActivatorNodeRef,
        handleProps: { ...attributes, ...listeners },
      })}
    </div>
  );
}
