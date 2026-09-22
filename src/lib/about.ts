// 软件宣传图 base64 内嵌（由 scripts/gen_asset_base64.mjs 从 promo_banner.jpg 生成）：
// dev server 关闭后弹窗图片仍可显示，不依赖运行中的静态服务器
import { promo_banner_base64 as promoBanner } from '../assets/promo-banner.base64';
import { version } from '../../package.json';

/** 应用品牌与关于信息（集中管理，改名只改这里）；版本号单一来源 package.json，随 sync_version.mjs 自动对齐 */
export const APP_NAME = 'AI Work 助手';
export const APP_VERSION = version;
export const APP_TAGLINE = '多账号签到与管理 · 一站式工作台';
export const APP_OVERVIEW =
  '多账号签到与管理一站式工作台（Web 版：React 18 + Rust axum 单体服务）。' +
  '支持 Trae 与 WorkBuddy 账号管理、签到、积分看板、定时调度与 OpenAI / Anthropic 兼容 API 网关，浏览器任意设备直访。';
export const APP_AUTHOR = '朱天伟';
export const APP_COPYRIGHT = `Copyright © 2026 ${APP_AUTHOR} · MIT License`;
export const APP_DISCLAIMER =
  '本工具与 Trae / WorkBuddy / 豆包等官方均无关联，仅供学习研究，请仅管理本人合法持有的账号，风险自担。';

export const LINK_GITHUB = 'https://github.com/smart-open';
export const LINK_BLOG = 'https://blog.sopenai.cn/';
export const LINK_REPO = 'https://github.com/smart-open/TraeWorkAssistant';

export { promoBanner };
