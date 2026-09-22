// AgentTrackerIsland hook 桥:Agent hook → 本地事件文件(零依赖,单文件)
// 安装后 command 形如:node "<path>/hook-bridge.js" <agent-id>
// argv[2] 是 Agent 标识(claude-code / codex / kimi-code),决定事件落盘文件
// events/<agent>.jsonl;缺省按 claude-code 处理(兼容旧版注入命令)。
// 事件名不作为参数——各 Agent(Claude Code/Codex/Kimi Code)的 stdin JSON
// 均带 hook_event_name 字段(2026-09-23 源码核实),从 stdin 读取即可。
//
// 设计约束(红线②故障隔离):只 append 本地文件即退出,不连端口、不找主进程;
// AgentTrackerIsland 未运行时本脚本依旧秒级成功,Agent 零感知。
const fs = require('fs');
const path = require('path');
const os = require('os');

// Agent 标识:命令行第二参数,仅允许字母/数字/连字符,防路径拼接被注入
const raw = process.argv[2] || 'claude-code';
const agent = /^[a-z0-9-]+$/.test(raw) ? raw : 'claude-code';

const eventsFile = path.join(
  process.env.LOCALAPPDATA || path.join(os.homedir(), 'AppData', 'Local'),
  'AgentTrackerIsland', 'events', agent + '.jsonl'
);

let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (c) => { buf += c; });
process.stdin.on('end', () => { writeEvent(buf); process.exit(0); });
// stdin 异常不结束时 2 秒兜底退出,绝不阻塞 Agent
setTimeout(() => { writeEvent(buf); process.exit(0); }, 2000).unref();

// 白名单提取字段写入事件行(不落对话内容,隐私最小化)
function writeEvent(raw) {
  try {
    const j = JSON.parse(raw || '{}');
    const pick = (k) => (typeof j[k] === 'string' && j[k] ? j[k] : undefined);
    const event = {
      ts: Date.now(),
      hook: pick('hook_event_name') || 'unknown',
      session_id: pick('session_id') || '',
      // 工具事件(PreToolUse/PostToolUse)与通知事件的专有字段
      tool_name: pick('tool_name'),
      // Notification 消息文本(额度/限流关键词判定);Kimi 失败事件的
      // 原因文本叫 errorMessage,一并归入 message(2026-09-23 源码核实)
      message: pick('message') || pick('errorMessage'),
    };
    fs.mkdirSync(path.dirname(eventsFile), { recursive: true });
    fs.appendFileSync(eventsFile, JSON.stringify(event) + '\n');
  } catch (e) { /* 静默失败:诊断信息写向 stderr 会污染 Agent */ }
}
