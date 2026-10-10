/**
 * 早期 invoke 重试（原 Settings.tsx 内联实现，M3-5 抽共享供常驻隐藏窗口共用）：
 * 本类窗口随应用启动即创建加载，挂载瞬间的 invoke 可能早于后端 setup 完成
 * （state 未 managed，报 "state not managed"），每 500ms 重试直至成功；超过
 * 上限放弃（真故障不该无限静默）。就绪后的后续调用（保存/刷新）不受此影响
 */
import { invoke } from "@tauri-apps/api/core";

export async function invokeReady<T>(cmd: string, tries = 8): Promise<T> {
  let lastErr: unknown;
  for (let i = 0; i < tries; i++) {
    if (i > 0) await new Promise((r) => setTimeout(r, 500));
    try {
      return await invoke<T>(cmd);
    } catch (e) {
      lastErr = e;
    }
  }
  throw lastErr;
}
