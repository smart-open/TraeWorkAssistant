/**
 * 指纹样本（issue #125 增强：一键从样本生成规则）的纯解析计算，抽出以便单测。
 *
 * 样本为后端 fingerprint_sample 输出，单行格式：
 *   system="<首条 system 前 300 字符（空白压缩、内嵌双引号已转单引号）>" tools=[工具名,...]
 * 解析失败时为字面量 "body 解析失败"。
 *
 * 一键生成规则的口径：
 * - from 必须逐字匹配请求原文（sanitizer 是 contains + replace 精确子串匹配），
 *   因此预填值**不加省略号、不做二次加工**；样本 system 文本经过空白压缩，
 *   多行长句可能失配——预填取首句（harness 身份声明句几乎都在最前，短句
 *   受空白压缩影响最小），用户可在规则行内校对编辑。首句切分跳过常见缩写
 *   句点（e.g. / i.e. / U.S. 等），避免 harness 句被拦腰截断成残句。
 * - to 默认走中性改写：已知 "powered by X Harness." 句式改写为中性句（保留
 *   语义去掉 harness 声明）；未命中回退空串（= 剥离，兜底最稳）。
 */

/** 从样本行提取 system 文本；非标准样本（如 "body 解析失败"）返回 null */
export function parseSampleSystem(sample: string): string | null {
  const m = sample.match(/^system="([\s\S]*?)"/);
  return m ? m[1] : null;
}

/** 中性改写策略表（一键生成 to 值）：由 neutralRewrite 按序全量应用（命中即改写，组合句式逐条清洗） */
export const NEUTRAL_REWRITE: Array<{ re: RegExp; to: string }> = [
  // issue #129：TraeCode harness 身份句——去掉 "in TraeCode" 品牌短语（与后端
  // default_template_map 同口径：to 零品牌词，避免改写句二次指纹化）
  { re: /\s+in TraeCode\b/g, to: '' },
  // issue #155：ZCode harness 身份句——去掉品牌词 ZCode（与后端默认表同口径：
  // "You are ZCode, ..." → "You are ..."、"interactive ZCode agent" → "interactive agent"）
  { re: /\bYou are ZCode, /g, to: 'You are ' },
  { re: /\binteractive ZCode agent\b/g, to: 'interactive agent' },
  // issue #125（DSH）："You are an AI agent powered by DeepSeek Harness." → "You are an AI agent."
  // 通用化：任意 "powered by <词组> Harness." 句式，句号保留在前半句
  { re: /\s+powered by [\w -]+ Harness\./g, to: '.' },
];

/** 常见缩写词（句点非句界）：首句截取跳过这些点，避免 harness 句被截成残句 */
const ABBREVIATIONS = new Set(['e.g', 'i.e', 'etc', 'vs', 'cf', 'al', 'approx', 'jr', 'sr', 'st']);

/** `. ` 处句点是否缩写点：词表命中，或连点/单字母缩写（U.S.、D.） */
function isAbbrevDot(text: string, dotIdx: number): boolean {
  const m = text.slice(0, dotIdx).match(/[A-Za-z.]+$/);
  if (!m) return false;
  const w = m[0].toLowerCase();
  if (ABBREVIATIONS.has(w)) return true;
  const segs = w.replace(/\.$/, '').split('.');
  return segs.every((s) => s.length === 1);
}

/** 预填 from：取 system 文本首句（首个非缩写 ". " 收尾，含句号）；无句界/
 * 不可解析时回退全文或 null（调用方对 null/空串禁用「生成规则」） */
export function extractFingerprintFrom(sample: string): string | null {
  const text = parseSampleSystem(sample);
  if (text == null) return null;
  let from = 0;
  for (;;) {
    const idx = text.indexOf('. ', from);
    if (idx < 0) return text;
    if (!isAbbrevDot(text, idx)) return text.slice(0, idx + 1);
    from = idx + 1;
  }
}

/** 一键生成 to：按序应用全部命中规则（组合句式逐条清洗，issue #155 双身份句连写），均未命中返回空串（= 剥离兜底） */
export function neutralRewrite(from: string): string {
  let out = from;
  for (const { re, to } of NEUTRAL_REWRITE) {
    out = out.replace(re, to);
  }
  return out === from ? '' : out;
}
