/**
 * 灵动岛背景不透明度：设置页【灵动岛】节单滑块调基准值（存储键 island_opacity，
 * 0–100 整数字符串），两层背景按固定偏移派生 alpha，视觉层级恒定——
 * 面板永远比胶囊实一档（文字密集，可读性要求更高）。
 * 应用方式与主题同构：派生 alpha 写根元素 CSS 变量，App.css 以
 * rgba(var(--pill-rgb), var(--pill-alpha)) 形式消费；深浅主题只换 RGB，alpha 通用。
 *
 * 贴边隐藏态不在此体系（2026-09-28 验收评审定论）：它是「胶囊本人被屏幕边缘
 * 遮住大半的局部」，按 Apple 式硬件物件处理——恒近黑不透明、不随壁纸/滑块变
 * （App.css .edge-tab 注释），弱化存在感靠「小而暗」而非半透明。
 */

/** 设置存储键（app_settings KV 表，纯前端约定，Rust 端通用透传） */
export const ISLAND_OPACITY_KEY = "island_opacity";

/** 设置页拖动后广播给岛窗口的事件名（emit 全窗口，岛窗口监听即时生效） */
export const ISLAND_OPACITY_EVENT = "island-opacity-changed";

/** 默认基准不透明度（%）：80（2026-10-10 所有者拍板，更通透的默认观感）。
 *  历史 72 沿自深色胶囊 alpha 0.72；已落库的老用户读存量值不受影响，
 *  仅未调过滑块的新用户拿到 80 */
export const ISLAND_OPACITY_DEFAULT = 80;

/** 滑块下限：60% 起（2026-10-10 所有者拍板，自 65 放宽，更通透）。
 *  按 WCAG「半透明表面取最低对比点」口径实算（合成底 = α·胶囊RGB + (1−α)·壁纸）：
 *  深色胶囊 RGB(20,20,24) α=0.30 叠纯白壁纸 → 主文字仅 1.64:1（故 30% 不可回退）；
 *  α=0.60 恰达 4:1（WCAG AA 大文字档）、4.5:1 需 α≥0.635——60 下限保证
 *  主文字最低仍约 4:1，极端壁纸下次要文字（text-4）不足 4.5 属 translucent
 *  HUD 固有约束（Apple 同类面板亦然），用户可拖动实时预览自行权衡 */
export const ISLAND_OPACITY_MIN = 60;
export const ISLAND_OPACITY_MAX = 100;

/** 派生偏移（alpha 绝对值）：面板比胶囊实一档 */
const PANEL_OFFSET = 0.16;

/** 收窄任意值为合法基准：非数字/越界一律钳到区间内（脏数据防御，参照 sanitizeThresholds） */
export function asIslandOpacity(v: unknown): number {
  const n = Number(v);
  if (!Number.isFinite(n)) return ISLAND_OPACITY_DEFAULT;
  return Math.min(ISLAND_OPACITY_MAX, Math.max(ISLAND_OPACITY_MIN, Math.round(n)));
}

/** 基准 % → 两层派生 alpha（clamp 到 0–1） */
export function deriveAlphas(base: number): { pill: number; panel: number } {
  const a = base / 100;
  const clamp01 = (x: number) => Math.min(1, Math.max(0, x));
  return {
    pill: clamp01(a),
    panel: clamp01(a + PANEL_OFFSET),
  };
}

/** 把派生 alpha 写到根元素 CSS 变量（inline style 优先级最高，主题切换只换 RGB 互不干扰） */
export function applyIslandOpacity(base: number): void {
  const { pill, panel } = deriveAlphas(base);
  const root = document.documentElement.style;
  root.setProperty("--pill-alpha", pill.toFixed(2));
  root.setProperty("--panel-alpha", panel.toFixed(2));
}
