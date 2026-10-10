/**
 * 供应商实例前端类型与命令封装（M3-4，06-PLAN §7）
 * 后端命令 10 个：provider_kinds / provider_kind_entries / provider_overview /
 * provider_account_create / update / delete / set_enabled / set_in_island /
 * test / env_creds_probe。
 * 厂商清单的唯一真值源是 Rust 静态注册表（provider_kinds），前端零硬编码；
 * 凭据真值永不经前端落库——plain 由后端写钥匙串，env 只传变量名。
 */
import { invoke } from "@tauri-apps/api/core";

/** 厂商定义（Rust 静态注册表镜像；报表筛选/额度页口径判定消费） */
export interface ProviderKindView {
  id: string;
  name: string;
  /** 品牌色（色条/色点/chip 高亮） */
  color: string;
  default_base: string;
  /** "windows"（窗口进度）| "balance"（金额）| "local_estimate"（五批） */
  quota_kind: string;
  currency: string;
  /** 凭据获取指引（表单 ⓘ 悬浮） */
  cred_hint: string;
  /** 本地推算厂商（M3-12 claude-local）：无凭据/无端点，表单隐藏凭据与接口地址行 */
  local_only: boolean;
}

/**
 * 选择器条目（2026-10-10 双站变体机制）：厂商注册表的变体展开形态——
 * 双站厂商展开为国内/国际两条（成对戴徽标），单站厂商一条无徽标。
 * 添加表单与厂商选择器的唯一数据源（KindPicker 消费）
 */
export interface KindEntryView {
  /** 选中后写入实例的 kind_id */
  kind_id: string;
  /** 变体键（"cn"/"intl"；单站厂商空串）——与 kind_id 组成条目唯一键 */
  variant_key: string;
  /** 条目主名（官方定名，不拼站别，如 Z.ai / Kimi 国际站） */
  name: string;
  /** kind 级显示名（编辑态反推不到变体时的兜底文案） */
  kind_name: string;
  /** 搜索别名（永不上屏，仅参与过滤） */
  aliases: string[];
  /** 站别徽标文案（空串＝不渲染徽标） */
  badge: string;
  /** 副行俗名（空串＝副行只显域名） */
  alt_name: string;
  /** 副行域名（default_base 的 host） */
  domain: string;
  /** 选中后写入的 base_override（默认变体=空串＝不回填；国际=完整端点必落库） */
  base: string;
  /** 所属 kind 的默认端点（保存归一化：base==它时存 null） */
  kind_default_base: string;
  /** 新建实例默认别名 */
  default_alias: string;
  /** 凭据指引（变体级覆盖 kind 级，如 Moonshot 国际站 401 警示） */
  cred_hint: string;
  color: string;
  quota_kind: string;
  currency: string;
  local_only: boolean;
}

/** 条目唯一键（同 kind 双站条目的区分键） */
export const kindEntryKey = (e: Pick<KindEntryView, "kind_id" | "variant_key">) =>
  e.variant_key ? `${e.kind_id}:${e.variant_key}` : e.kind_id;

export const listKindEntries = () => invoke<KindEntryView[]>("provider_kind_entries");

/** 实例最新余额快照（设置页卡片摘要；岛金额 chip 为 M3-6 范围） */
export interface BalanceView {
  currency: string;
  total: number;
  granted: number | null;
  fetched_at: number;
}

/** 实例卡片视图（设置页列表数据源；M3-5 额度页复用同一命令） */
export interface AccountOverview {
  id: string;
  kind_id: string;
  /** 运行时厂商显示名（2026-10-10 双站变体投影：Z.ai 实例显示 Z.ai 非 GLM） */
  kind_name: string;
  alias: string;
  base_override: string | null;
  /** 'plain'（真值在钥匙串，不回显）| 'env'（cred_value 为变量名，非敏感） */
  cred_kind: string;
  cred_value: string | null;
  /** 备注（discovered 实例的来源留痕） */
  note: string | null;
  enabled: boolean;
  in_island: boolean;
  /** 'manual' | 'discovered'（卡片来源徽标） */
  origin: string;
  created_at: number;
  updated_at: number;
  /** 凭据解析状态：null=可用；非空=分类文案（卡片红字，与网络失败区分） */
  cred_error: string | null;
  /** 脱敏凭据回显（前 8＋…＋尾 4 三档规则）：编辑表单「认 key」用；真值不进前端 */
  cred_masked: string | null;
  /** 该实例最新窗口快照（Windows 口径） */
  quotas: import("./types").QuotaView[];
  /** 该实例最新余额快照（Balance 口径） */
  balance: BalanceView | null;
}

/** 检测三态的"结果"档（转圈由调用方 busy 态表达） */
export interface TestResult {
  ok: boolean;
  text: string;
}

export const listAccounts = () => invoke<AccountOverview[]>("provider_overview");

/** 创建实例：plain 传 key（真值由后端写钥匙串），env 传 envVar（存变量名） */
export const createAccount = (args: {
  kindId: string;
  alias: string;
  credMode: "plain" | "env";
  envVar?: string | null;
  key?: string | null;
  baseOverride?: string | null;
  inIsland: boolean;
}) => invoke<string>("provider_account_create", args);

/** 编辑实例（厂商锁定不可传）：key/envVar 为 null 表示凭据不变更 */
export const updateAccount = (args: {
  id: string;
  alias: string;
  baseOverride?: string | null;
  credMode: "plain" | "env";
  envVar?: string | null;
  key?: string | null;
  note?: string | null;
}) => invoke<void>("provider_account_update", args);

export const deleteAccount = (id: string) => invoke<void>("provider_account_delete", { id });

export const setAccountEnabled = (id: string, enabled: boolean) =>
  invoke<void>("provider_account_set_enabled", { id, enabled });

export const setAccountInIsland = (id: string, inIsland: boolean) =>
  invoke<void>("provider_account_set_in_island", { id, inIsland });

/** 拖拽排序落库（2026-10-03）：全量 id 数组整体重写 sort_order（0011 迁移列），
 *  列表顺序是岛轮播/托盘/额度页/报表曲线的全局唯一顺序源 */
export const reorderAccounts = (ids: string[]) =>
  invoke<void>("provider_account_reorder", { ids });

/** 连接检测：返回人类可读摘要（如"余额 ¥112.40"）；失败 Err 为分类文案 */
export const testAccount = (id: string) => invoke<string>("provider_account_test", { id });

/** 单卡刷新（M3-5 额度页）：与检测不同——快照真实落库，刷新后重拉
 *  overview 可见数据与时间戳推进；失败 Err 为分类文案 */
export const refreshAccount = (id: string) =>
  invoke<string>("provider_account_refresh", { id });

/** 一键复制完整 Key（2026-10-10）：后端读钥匙串写剪贴板（真值不进前端），
 *  120s 后读回比对自动清空；失败 Err 为分类文案 */
export const copyAccountKey = (id: string) =>
  invoke<void>("provider_account_copy_key", { id });

/** env 变量就地解析检测（表单失焦反馈；两层检测，未继承提示重启生效） */
export const probeEnvVar = (varName: string) =>
  invoke<string>("env_creds_probe", { var: varName });
