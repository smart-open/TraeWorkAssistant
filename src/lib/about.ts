// 赞赏码 base64 内嵌（由 scripts/gen_asset_base64.py 从 donate-qr.jpg 生成）：
// dev server 关闭后弹窗图片仍可显示，不依赖运行中的静态服务器
import { donate_qr_base64 as donateQr } from '../assets/donate-qr.base64';

/** 应用品牌与关于信息（集中管理，改名/升版只改这里） */
export const APP_NAME = 'AI Work 助手';
export const APP_VERSION = '3.1.0';
export const APP_TAGLINE = '多账号签到与管理 · 一站式工作台';
export const APP_OVERVIEW =
  'Windows 桌面端多账号签到与管理工具（Tauri 2 + React 18 + Rust）。' +
  '支持多账号签到、登录态切换、设备隔离、积分看板与 OpenAI 兼容 API 网关，数据全部本地存储。';
export const APP_AUTHOR = '朱天伟';
export const APP_COPYRIGHT = `Copyright © 2026 ${APP_AUTHOR} · MIT License`;
export const APP_DISCLAIMER =
  '本工具与 Trae / WorkBuddy / 豆包等官方均无关联，仅供学习研究，请仅管理本人合法持有的账号，风险自担。';

export const LINK_GITHUB = 'https://github.com/smart-open';
export const LINK_BLOG = 'https://blog.sopenai.cn/';
export const LINK_REPO = 'https://github.com/smart-open/TraeWorkAssistant';

export { donateQr };
