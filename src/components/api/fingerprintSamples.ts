// 指纹样本 → 清洗规则草案（issue #125 增强：闭环最省力的一环）
// 网关命中样本形如：
//   system="You are opencode, an interactive CLI tool that helps users with software engineering tasks." tools=[\"bash\",\"read\"]
// 上游逐字匹配的是这条整句（含工具清单）——只截取片段（如 ToB 前缀）会让规则
// 命中半句、改写出破句，必须整句录入。

export type SampleTier = 'wb' | 'buddy-ai';

export class FingerprintSampleParseError extends Error {}

/**
 * 样本来源池 → 所属清洗档位（§4.3 分区清洗）。清洗规则表按档位隔离存储，
 * 把 A 档样本生成的规则写进 B 档表只会得到一条永不命中的死规则。
 * null = 该池请求路径不做模板改写（qoder 上游无清洗环节），样本仅作诊断。
 */
export function sampleTier(pool: string): SampleTier | null {
  if (pool === 'qoder') return null;
  if (pool === 'buddy-ai') return 'buddy-ai';
  // trae（routes.rs）/ buddy / wb（wb_route 池标识）共用 wb 档规则表
  return 'wb';
}

/** 解析样本中的 system 文本（system="…" 与 tools=[…] 两种段落顺序都兼容）。
 * 后端在原文超 300 字符时于引号内尾部补 "…" 截断标记（区分「首句完整」与「残句」）
 * ——此处剥离该标记，避免把省略号混进 from（sanitizer 是逐字 contains 匹配，
 * 带省略号的 from 必然落空） */
export function parseSystemText(sample: string): string {
  const m = /system="([\s\S]*?)"\s*(?:tools=|$)/.exec(sample.trim());
  if (!m) throw new FingerprintSampleParseError('未找到 system="…" 段落');
  return m[1].replace(/…$/, '');
}

/** 解析可安全改写的前缀：首英文逗号/句号处截断（保留标点前的整段，含末尾空格不留） */
export function parsePrefix(sample: string): string {
  let text = parseSystemText(sample);
  let cut = -1;
  for (let i = 0; i < text.length; i += 1) {
    const c = text[i];
    if ((c === ',' || c === '.') && i > 0 && /[A-Za-z]/.test(text[i - 1])) {
      cut = i;
      break;
    }
  }
  if (cut >= 0) text = text.slice(0, cut);
  // 归一化：仅去除首尾空白（内部空格是英文行文的一部分，删掉会破词）
  text = text.trim();
  if (!text) throw new FingerprintSampleParseError('无可改写前缀');
  return text;
}
