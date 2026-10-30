import { describe, expect, it } from 'vitest';

import { extractFingerprintFrom, neutralRewrite, parseSampleSystem } from './fingerprintSamples';

describe('parseSampleSystem', () => {
  it('标准样本（带 tools 尾巴）提取 system 文本', () => {
    const sample =
      'system="You are an AI agent powered by DeepSeek Harness. Follow user." tools=[mcp__ide__g]';
    expect(parseSampleSystem(sample)).toBe(
      'You are an AI agent powered by DeepSeek Harness. Follow user.',
    );
  });

  it('无 tools 尾巴的样本同样提取', () => {
    expect(parseSampleSystem('system="abc"')).toBe('abc');
  });

  it('空 system 文本返回空串（合法解析）', () => {
    expect(parseSampleSystem('system=""')).toBe('');
  });

  it('body 解析失败等非标准样本返回 null', () => {
    expect(parseSampleSystem('body 解析失败')).toBeNull();
  });
});

describe('extractFingerprintFrom', () => {
  it('取首句（harness 身份声明句几乎都在最前）', () => {
    expect(
      extractFingerprintFrom(
        'system="You are an AI agent powered by DeepSeek Harness. Follow instructions." tools=[x]',
      ),
    ).toBe('You are an AI agent powered by DeepSeek Harness.');
  });

  it('无句界（句号收尾无后文）回退全文', () => {
    expect(extractFingerprintFrom('system="You are an AI agent powered by DeepSeek Harness."')).toBe(
      'You are an AI agent powered by DeepSeek Harness.',
    );
  });

  it('不可解析样本透传 null', () => {
    expect(extractFingerprintFrom('body 解析失败')).toBeNull();
  });

  it('缩写句点（e.g.）不截断，取完整 harness 句', () => {
    expect(
      extractFingerprintFrom(
        'system="You are an AI agent, e.g. a coding assistant powered by DeepSeek Harness. You follow user instructions." tools=[a]',
      ),
    ).toBe('You are an AI agent, e.g. a coding assistant powered by DeepSeek Harness.');
  });

  it('缩写句点（U.S.）不截断，跳到真正的句界', () => {
    expect(extractFingerprintFrom('system="Serves U.S. users. Powered by X Harness. Rest."')).toBe(
      'Serves U.S. users.',
    );
  });

  it('提取的 from 与 neutralRewrite 组合改写句式', () => {
    expect(neutralRewrite('You are an AI agent, e.g. a coding assistant powered by DeepSeek Harness.')).toBe(
      'You are an AI agent, e.g. a coding assistant.',
    );
  });
});

describe('neutralRewrite', () => {
  it('DSH 句式改写为中性句（句号保留）', () => {
    expect(neutralRewrite('You are an AI agent powered by DeepSeek Harness.')).toBe(
      'You are an AI agent.',
    );
  });

  it('任意 powered by X Harness 句式通用命中', () => {
    expect(neutralRewrite('You are an AI assistant powered by Foo Bar Harness.')).toBe(
      'You are an AI assistant.',
    );
  });

  it('issue #129：TraeCode 身份句去品牌短语后零残留', () => {
    expect(
      neutralRewrite(
        'You are an interactive agent in TraeCode that helps the USER with software engineering tasks.',
      ),
    ).toBe('You are an interactive agent that helps the USER with software engineering tasks.');
  });

  it('issue #155：ZCode 身份句去品牌词后零残留', () => {
    // 首句："You are ZCode, an interactive coding agent" → "You are an interactive coding agent"
    expect(neutralRewrite('You are ZCode, an interactive coding agent')).toBe(
      'You are an interactive coding agent',
    );
    // 第二身份句："interactive ZCode agent" → "interactive agent"
    expect(
      neutralRewrite('You are an interactive ZCode agent that helps users with software engineering tasks'),
    ).toBe('You are an interactive agent that helps users with software engineering tasks');
    // issue #155 实证样本：双身份句之间无句号连写，extractFingerprintFrom 按
    // 首个 '. ' 截取会把两句并成一个 from——改写须逐条应用全部命中规则，两句
    // 都清洗、零 ZCode 残留（与后端 apply_template_map_seeded 全量应用同口径）
    expect(
      neutralRewrite(
        'You are ZCode, an interactive coding agent You are an interactive ZCode agent that helps users with software engineering tasks',
      ),
    ).toBe(
      'You are an interactive coding agent You are an interactive agent that helps users with software engineering tasks',
    );
  });

  it('未命中回退空串（剥离兜底）', () => {
    expect(neutralRewrite("You are Claude Code, Anthropic's official CLI for Claude.")).toBe('');
    expect(neutralRewrite('')).toBe('');
  });
});
