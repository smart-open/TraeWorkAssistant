// 赞赏码 base64 内嵌（来自 src/assets/donate-qr.base64.ts）：
// dev server 关闭后弹窗图片仍可显示，不依赖运行中的静态服务器
import { donate_qr_base64 as donateQr } from '../assets/donate-qr.base64';

/** 应用品牌与关于信息（集中管理，改名/升版只改这里） */
export const APP_NAME = 'Trae Work Assistant';
export const APP_VERSION = '2.4.6';
export const APP_TAGLINE = 'Windows 桌面端多账号签到与管理工具';
export const APP_OVERVIEW =
  '基于 Tauri 2 + React 18 + Rust 的桌面工具。' +
  '支持多账号管理、登录态切换、一键签到、积分看板、本地 MITM 代理自动捕获 JWT、' +
  '6 层设备标识重置与 OpenAI 兼容 API 网关，数据全部本地存储。';
export const APP_AUTHOR = '朱天伟';
export const APP_COPYRIGHT = `Copyright © 2026 ${APP_AUTHOR} · MIT License`;
export const APP_DISCLAIMER =
  '本工具与 Trae Work 官方无任何关联，仅供学习研究。使用本工具可能违反 Trae Work 服务条款，风险自担，请仅管理本人合法持有的账号。';

export const LINK_GITHUB = 'https://github.com/smart-open';
export const LINK_BLOG = 'https://blog.sopenai.cn/';
export const LINK_REPO = 'https://github.com/smart-open/TraeWorkAssistant';

export { donateQr };
