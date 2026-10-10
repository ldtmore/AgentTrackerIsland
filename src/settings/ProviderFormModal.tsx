/**
 * 添加/编辑供应商弹窗（M3-4 UX 改版，2026-10-03 四轮审查拆分自 Settings.tsx）：
 * 页内模态而非系统窗口——设置窗口本身已是辅助弹窗层级，页内遮罩视觉等效且
 * 免去跨窗口数据传递；Esc / 点遮罩 / 取消按钮均可关闭（用户主动发起的编辑容器，
 * 非打断式弹窗）。
 * 拆分动机：表单 10 个状态只在弹窗打开期间有意义——组件挂载即初始化、
 * 关闭即销毁，原先在父组件里靠 openCreate/openEdit 手工搬 9~10 个 set。
 */
import { useEffect, useRef, useState } from "react";
import { useFocusTrap } from "../shared/useFocusTrap";
import { CopyIcon, InfoIcon } from "../shared/icons";
import {
  copyAccountKey,
  createAccount,
  kindEntryKey,
  probeEnvVar,
  updateAccount,
  type AccountOverview,
  type KindEntryView,
  type TestResult,
} from "../shared/providerTypes";
import { blurOnEnter, EyeIcon, KindPicker } from "./widgets";

/**
 * 编辑态条目反推（2026-10-10 双站变体机制）：按实例 kind_id＋base_override
 * 反推所属选择器条目——空覆盖/默认端点→默认条目；端点恰等于某变体→该条目；
 * 自定义域名→kind 默认条目（锁定文案回退 kind 名并注明自定义地址，不强行
 * 归组）。归一化与后端 variant_of 同口径（trim＋去尾斜杠＋大小写不敏感）
 */
const pickEntry = (
  entries: KindEntryView[],
  kindId: string,
  baseOverride: string | null,
): KindEntryView | null => {
  const same = entries.filter((e) => e.kind_id === kindId);
  if (same.length === 0) return null;
  const norm = (s: string) => s.trim().replace(/\/+$/, "").toLowerCase();
  const b = baseOverride?.trim() ?? "";
  if (!b || norm(b) === norm(same[0].kind_default_base)) {
    return same.find((e) => !e.variant_key || e.variant_key === "cn") ?? same[0];
  }
  return same.find((e) => e.base && norm(e.base) === norm(b)) ?? same[0];
};

export default function ProviderFormModal({
  entries,
  accounts,
  editing,
  onClose,
  onSaved,
  runTest,
  showToast,
}: {
  /** 选择器条目清单（厂商注册表变体展开投影；唯一真值源在后端） */
  entries: KindEntryView[];
  /** 现有实例（别名冲突即时校验用） */
  accounts: AccountOverview[];
  /** 编辑目标；null＝新增 */
  editing: AccountOverview | null;
  /** 关闭弹窗（无论保存与否） */
  onClose: () => void;
  /** 保存成功：父组件重拉实例列表＋广播 provider-accounts-changed */
  onSaved: () => void;
  /** 保存后自动检测（三态反馈落在实例卡片上） */
  runTest: (id: string) => void;
  showToast: (text: string, kind: "ok" | "error") => void;
}) {
  // —— 表单状态（挂载即初始化：新增取第一个条目、编辑按 base 反推变体条目） ——
  const [fEntry, setFEntry] = useState<KindEntryView | null>(
    editing
      ? pickEntry(entries, editing.kind_id, editing.base_override)
      : entries[0] ?? null,
  );
  const [fAlias, setFAlias] = useState(
    editing ? editing.alias : entries[0]?.default_alias ?? "",
  );
  const [fCredMode, setFCredMode] = useState<"plain" | "env">(
    editing && editing.cred_kind === "env" ? "env" : "plain",
  );
  const [fSecret, setFSecret] = useState(
    // 明文 Key / env 变量名共用输入；明文不回显（留空 = 不修改），env 回显变量名
    editing && editing.cred_kind === "env" ? editing.cred_value ?? "" : "",
  );
  const [fShowSecret, setFShowSecret] = useState(false);
  const [fBase, setFBase] = useState(editing ? editing.base_override ?? "" : "");
  const [formError, setFormError] = useState(""); // 表单级错误（凭据必填/后端拒绝等）
  // 编辑态 base 是否为自定义值（反推不到变体条目）：锁定文案回退 kind 名并注明
  const baseCustomized =
    !!editing &&
    !!fEntry &&
    entries.some((e) => e.kind_id === fEntry.kind_id && e.variant_key) &&
    !!editing.base_override?.trim() &&
    !entries.some(
      (e) =>
        e.kind_id === editing.kind_id &&
        !!e.base &&
        e.base.trim().replace(/\/+$/, "").toLowerCase() ===
          editing.base_override!.trim().replace(/\/+$/, "").toLowerCase(),
    );
  // 明文 Key 的脱敏回显（编辑态专用）：框内显示物——fSecret 状态恒为用户
  // 真实输入（空＝不修改），渲染用 fSecret || maskedDisplay 合成，杜绝
  // 「掩码被当新 Key 保存」；凭据不可读时为空（placeholder 引导重贴）
  const maskedDisplay =
    editing && fCredMode === "plain" ? editing.cred_masked ?? "" : "";
  // env 变量名失焦就地解析检测（✓ 脱敏 / ✗ 分类提示——D13 两层检测）
  const [probe, setProbe] = useState<TestResult | null>(null);
  // 保存进行中（防连点）
  const [saving, setSaving] = useState(false);

  // 弹窗打开期间：Esc 关闭 + 焦点管理（页内模态的基础交互）。
  // useFocusTrap（2026-10-03 审查新增）：打开初始聚焦/Tab 首尾循环/关闭归焦——
  // 此前 aria-modal 是虚假声明，键盘用户会 Tab 穿透到遮罩下的整页设置行。
  // 原先这里的 document.body.style.overflow 锁是无效代码（07-UX 2.2 删除）：
  // 本页 body 恒 overflow:hidden，真正的滚动容器是 .st-page——滚动穿透已由
  // .st-modal-overlay 的 overscroll-behavior:contain 在 CSS 层根治
  const formRef = useRef<HTMLDivElement>(null);
  useFocusTrap(formRef, true);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  /** 别名即时冲突校验：全局唯一（2026-09-30 D14 升级，跨厂商也拦，后端 0009
   *  索引兜底），编辑态排除自身 */
  const aliasConflict = accounts.some(
    (a) => a.id !== editing?.id && a.alias === fAlias.trim(),
  );

  /** 自定义中转站（M3-10）：无默认端点，接口地址必填（后端 create/update 同规则兜底） */
  const baseRequired = fEntry?.kind_id === "custom-openai";
  /** 本地推算厂商（M3-12 claude-local）：无凭据/无端点，表单隐藏凭据与接口地址行 */
  const localOnly = fEntry?.local_only === true;

  /** env 变量名失焦就地解析（两层检测：进程 env → 注册表，未继承提示重启生效） */
  const probeEnv = async () => {
    const name = fSecret.trim();
    if (!name || fCredMode !== "env") return;
    try {
      const masked = await probeEnvVar(name);
      setProbe({ ok: true, text: `已解析 ${masked}` });
    } catch (e) {
      setProbe({ ok: false, text: String(e) });
    }
  };

  /** 保存并检测：保存成功即生效（调度器热刷新 1~5s 内自动生效），随后自动检测
   *  一次（三态反馈显示在实例卡片上）；检测失败不阻塞保存（06 §7.2）。
   *  base 归一化（2026-10-10）：空值或恰等于 kind 默认端点时存 null——保留
   *  「厂商换默认端点自动跟随」语义，不把今天的默认值冻结进实例 */
  const saveAndTest = async () => {
    if (!fEntry || saving) return;
    const alias = fAlias.trim();
    const secret = fSecret.trim();
    if (!alias) {
      setFormError("别名不能为空");
      return;
    }
    if (aliasConflict) {
      setFormError("别名已存在（全局唯一）");
      return;
    }
    // 凭据校验：env 变量名恒必填（存库的就是它）；明文在「新增」或「原 env 切回
    // 明文」时必填（env 实例无钥匙串条目，切回明文不填 = 凭据丢失），原明文留空 = 不修改；
    // 本地推算厂商（M3-12）无凭据概念整段跳过
    if (!localOnly) {
      if (fCredMode === "env" && !secret) {
        setFormError("请填写环境变量名");
        return;
      }
      if (fCredMode === "plain" && !secret && (!editing || editing.cred_kind === "env")) {
        setFormError("请填写 API Key");
        return;
      }
      if (baseRequired && !fBase.trim()) {
        setFormError("自定义中转必须填写接口地址（base）");
        return;
      }
    }
    const baseOverride =
      !fBase.trim() || fBase.trim() === fEntry.kind_default_base ? null : fBase.trim();
    setSaving(true);
    setFormError("");
    try {
      let id: string;
      if (editing) {
        await updateAccount({
          id: editing.id,
          alias,
          baseOverride,
          credMode: fCredMode,
          envVar: fCredMode === "env" ? secret : null,
          key: fCredMode === "plain" && secret ? secret : null,
          note: editing.note,
        });
        id = editing.id;
      } else {
        id = await createAccount({
          kindId: fEntry.kind_id,
          alias,
          credMode: fCredMode,
          envVar: fCredMode === "env" ? secret : null,
          key: fCredMode === "plain" ? secret : null,
          baseOverride,
          inIsland: true,
        });
      }
      onClose();
      showToast(
        editing ? "已保存，改动即时生效" : `已添加「${alias}」，额度查询即将自动开始`,
        "ok",
      );
      onSaved();
      void runTest(id); // 保存后自动检测，结果落在卡片上
    } catch (e) {
      setFormError(String(e));
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="st-modal-overlay" onClick={onClose}>
      <div
        className="st-modal"
        role="dialog"
        aria-modal="true"
        aria-label={editing ? "编辑供应商" : "添加供应商"}
        ref={formRef}
        onClick={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          // Enter 提交（2026-10-03 审查补充）：焦点在按钮/非输入元素上时回车
          // 直接触发保存检测（输入框内回车维持原失焦提交路径，避免双动作）
          if (e.key !== "Enter") return;
          const t = e.target as HTMLElement;
          if (t instanceof HTMLInputElement || t instanceof HTMLTextAreaElement) return;
          if (t instanceof HTMLButtonElement && t.type !== "submit") {
            // 按钮上回车交给按钮自身行为（取消/关闭等），主操作按钮除外。
            // 结构判断（四轮审查）：原先按文案「保存」字符串匹配，按钮改文案
            // （如「确定」「添加」）会让 Enter 提交静默失效
            if (!t.dataset.primary) return;
          }
          e.preventDefault();
          saveAndTest();
        }}
      >
        <div className="st-modal-title">
          {editing ? `编辑「${editing.alias}」` : "添加供应商"}
        </div>
        <div className="st-modal-body">
          <div className="st-form-row">
            <span className="st-form-label">厂商</span>
            <div className="st-form-field">
              {editing && fEntry ? (
                // 编辑态锁定：只展示反推出的当前条目（双站变体按 base 反推，
                // 自定义地址回退 kind 名并注明）；展示也选不了的置灰 chip 是反模式
                <div className="st-kind-locked">
                  <i className="st-chip-dot" style={{ background: fEntry.color }} />
                  {baseCustomized ? fEntry.kind_name : fEntry.name}
                  {fEntry.badge && !baseCustomized && (
                    <span className="st-kind-badge">{fEntry.badge}</span>
                  )}
                  <span className="st-form-hint">
                    {baseCustomized
                      ? "接口地址为自定义值，创建后不可更改厂商与站点，如需更换请删除后重新添加"
                      : "创建后不可更改，如需更换请删除后重新添加"}
                  </span>
                </div>
              ) : (
                <KindPicker
                  entries={entries}
                  value={fEntry ? kindEntryKey(fEntry) : ""}
                  onChange={(e) => {
                    setFEntry(e);
                    // 切换条目带出该站默认别名与端点（国际站显式回填 base，
                    // 默认站留空＝kind 默认语义，保留厂商换域名自动跟随）
                    setFAlias(e.default_alias);
                    setFBase(e.base);
                  }}
                />
              )}
              <div className="st-form-hint">
                <span className="st-row-tip" aria-label="凭据获取指引">
                  <InfoIcon />
                </span>
                {fEntry?.cred_hint}
              </div>
            </div>
          </div>
          <div className="st-form-row">
            <span className="st-form-label">别名</span>
            <div className="st-form-field">
              <input
                className={`st-input${aliasConflict ? " st-input-err" : ""}`}
                value={fAlias}
                placeholder={fEntry?.default_alias}
                maxLength={50}
                onChange={(e) => setFAlias(e.target.value)}
                aria-label="实例别名"
              />
              {aliasConflict && <span className="st-form-err">别名已存在（全局唯一）</span>}
            </div>
          </div>
          {localOnly ? (
            /* 本地推算厂商（M3-12）：无凭据/无端点，显示说明行替代两个输入行。
               hint 加 aligned 修饰补 label 的 padding-top（label 的 7px 顶边距
               是为 input 行设计的，纯文字行不加会基线错位） */
            <div className="st-form-row">
              <span className="st-form-label">凭据</span>
              <div className="st-form-field">
                <span className="st-form-hint st-form-hint-aligned">
                  本地推算供应商无需凭据与接口地址——直接聚合本机 Claude Code 用量
                </span>
              </div>
            </div>
          ) : (
            <>
              <div className="st-form-row">
                <span className="st-form-label">凭据</span>
                <div className="st-form-field">
                  {/* 类型选择与值输入一体框（前缀组合模式）：左下拉切类型，
                      右侧输入框随类型联动形态。
                      脱敏回显（2026-10-10）：编辑态未输入时框内显示后端脱敏值
                      （渲染层合成——fSecret 状态恒为用户真实输入、空＝不修改，
                      提交零改动，杜绝「掩码当新 Key 保存」）；一旦输入即整体
                      替换为新 Key。掩码已脱敏，以明文形态显示（转圆点反失辨识） */}
                  <div className="st-combo">
                    <select
                      className="st-combo-select"
                      value={fCredMode}
                      onChange={(e) => {
                        setFCredMode(e.target.value as "plain" | "env");
                        setFSecret("");
                        setProbe(null);
                      }}
                      aria-label="凭据类型"
                    >
                      <option value="plain">明文 Key</option>
                      <option value="env">环境变量</option>
                    </select>
                    <i className="st-combo-sep" />
                    <input
                      className="st-combo-input"
                      type={fCredMode === "plain" && fSecret && !fShowSecret ? "password" : "text"}
                      value={fSecret || maskedDisplay}
                      maxLength={fCredMode === "env" ? 128 : 500}
                      placeholder={
                        editing && fCredMode === "plain"
                          ? maskedDisplay
                            ? "直接粘贴新 Key 即可更换"
                            : "凭据不可读，请重新粘贴 Key"
                          : fCredMode === "env"
                            ? "如 DEEPSEEK_API_KEY"
                            : "粘贴 API Key"
                      }
                      onChange={(e) => {
                        setFSecret(e.target.value);
                        setProbe(null);
                      }}
                      onBlur={fCredMode === "env" ? probeEnv : undefined}
                      onKeyDown={blurOnEnter}
                      aria-label={fCredMode === "plain" ? "API Key" : "环境变量名"}
                    />
                    {/* 复制完整 Key（copy without reveal）：后端读钥匙串写剪贴板
                        （真值不进前端），120s 后读回比对自动清空——仅编辑态有 */}
                    {editing && fCredMode === "plain" && (
                      <button
                        type="button"
                        className="st-key-copy"
                        title="复制完整 Key（120 秒后自动清空剪贴板）"
                        aria-label="复制完整 Key"
                        onClick={async () => {
                          try {
                            await copyAccountKey(editing.id);
                            showToast("已复制完整 Key，120 秒后自动清空剪贴板", "ok");
                          } catch (e) {
                            showToast(String(e), "error");
                          }
                        }}
                      >
                        <CopyIcon />
                      </button>
                    )}
                    {/* 眼睛仅在有真实输入时出现：未输入态框内是脱敏值（无明文
                        可切），此时显示眼睛会让用户把掩码误当 Key 明文 */}
                    {fCredMode === "plain" && fSecret && (
                      <button
                        type="button"
                        className="st-eye"
                        title={fShowSecret ? "隐藏" : "显示"}
                        onClick={() => setFShowSecret(!fShowSecret)}
                      >
                        <EyeIcon off={!fShowSecret} />
                      </button>
                    )}
                  </div>
                  {fCredMode === "env" && probe && (
                    <span className={probe.ok ? "st-form-ok" : "st-form-err"}>
                      {probe.ok ? "✓ " : "✗ "}
                      {probe.text}
                    </span>
                  )}
                  <span className="st-form-hint">
                    {fCredMode === "plain"
                      ? `${
                          editing
                            ? "保持原样＝不修改（框内为脱敏回显，非真实 Key）；"
                            : ""
                        }仅保存到系统凭据管理器，不上传`
                      : "终端会话临时设置的变量对常驻应用无效；新设变量后需重启应用"}
                  </span>
                </div>
              </div>
              <div className="st-form-row">
                <span className="st-form-label">
                  接口地址{baseRequired && <span className="st-form-err">＊</span>}
                </span>
                <div className="st-form-field">
                  {baseRequired ? (
                    /* 自定义中转站（M3-10）：无默认端点，必填显式输入 */
                    <input
                      className="st-input"
                      value={fBase}
                      maxLength={300}
                      placeholder="站点根地址，如 https://relay.example.com"
                      onChange={(e) => setFBase(e.target.value)}
                      aria-label="接口地址（必填）"
                    />
                  ) : (
                    /* 接口地址恒平铺（2026-10-10 用户拍板：去掉折叠）——保存前
                       眼见为实请求发往哪里；默认站留空（placeholder 显完整默认
                       端点），国际站选中时已显式回填实值 */
                    <input
                      className="st-input"
                      value={fBase}
                      maxLength={300}
                      placeholder={
                        fEntry?.kind_default_base
                          ? `默认 ${fEntry.kind_default_base}，留空即用默认`
                          : "默认端点"
                      }
                      onChange={(e) => setFBase(e.target.value)}
                      aria-label="接口地址覆盖"
                    />
                  )}
                </div>
              </div>
            </>
          )}
          {formError && <div className="st-form-err">{formError}</div>}
        </div>
        <div className="st-modal-actions">
          <button type="button" className="st-btn st-btn-sm" onClick={onClose}>
            取消
          </button>
          <button
            type="button"
            className="st-btn st-btn-primary"
            data-primary="1"
            disabled={saving}
            onClick={saveAndTest}
          >
            {saving ? "保存中…" : "保存并检测"}
          </button>
        </div>
      </div>
    </div>
  );
}
