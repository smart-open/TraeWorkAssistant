import {
  Sparkles,
  Bot,
  Boxes,
  LayoutGrid,
  Terminal,
  Code,
  Cpu,
  Rocket,
  Zap,
  Star,
  Heart,
  Coffee,
  Ghost,
  Rabbit,
  Turtle,
  PawPrint,
  Cat,
  Dog,
  Fish,
  Bird,
  Bug,
  Flower2,
  TreePine,
  Mountain,
  Waves,
  Flame,
  Snowflake,
  Moon,
  Sun,
  Cloud,
  Rainbow,
  Gem,
  Crown,
  Orbit,
  Atom,
  Binary,
  Braces,
  Command,
  Gamepad2,
  Joystick,
  Dices,
  Puzzle,
  Blocks,
  Shapes,
  Hexagon,
  Circle,
  Square,
  Triangle,
  Diamond,
  type LucideIcon,
} from 'lucide-react';
import type { AppKey } from '../types';

/**
 * 侧边栏应用图标候选表（仅 UI 偏好：settings.app_icons 存图标名，非法/缺失回退默认）。
 * 独立成模块，避免 store ↔ Sidebar 循环依赖
 * （store 需兜底归一 app_icons，而 APP_TABS 默认图标定义在 Sidebar）。
 */
export const ICON_CHOICES: { name: string; icon: LucideIcon }[] = [
  { name: 'Sparkles', icon: Sparkles },
  { name: 'Bot', icon: Bot },
  { name: 'Boxes', icon: Boxes },
  { name: 'LayoutGrid', icon: LayoutGrid },
  { name: 'Terminal', icon: Terminal },
  { name: 'Code', icon: Code },
  { name: 'Cpu', icon: Cpu },
  { name: 'Rocket', icon: Rocket },
  { name: 'Zap', icon: Zap },
  { name: 'Star', icon: Star },
  { name: 'Heart', icon: Heart },
  { name: 'Coffee', icon: Coffee },
  { name: 'Ghost', icon: Ghost },
  { name: 'Rabbit', icon: Rabbit },
  { name: 'Turtle', icon: Turtle },
  { name: 'PawPrint', icon: PawPrint },
  { name: 'Cat', icon: Cat },
  { name: 'Dog', icon: Dog },
  { name: 'Fish', icon: Fish },
  { name: 'Bird', icon: Bird },
  { name: 'Bug', icon: Bug },
  { name: 'Flower2', icon: Flower2 },
  { name: 'TreePine', icon: TreePine },
  { name: 'Mountain', icon: Mountain },
  { name: 'Waves', icon: Waves },
  { name: 'Flame', icon: Flame },
  { name: 'Snowflake', icon: Snowflake },
  { name: 'Moon', icon: Moon },
  { name: 'Sun', icon: Sun },
  { name: 'Cloud', icon: Cloud },
  { name: 'Rainbow', icon: Rainbow },
  { name: 'Gem', icon: Gem },
  { name: 'Crown', icon: Crown },
  { name: 'Orbit', icon: Orbit },
  { name: 'Atom', icon: Atom },
  { name: 'Binary', icon: Binary },
  { name: 'Braces', icon: Braces },
  { name: 'Command', icon: Command },
  { name: 'Gamepad2', icon: Gamepad2 },
  { name: 'Joystick', icon: Joystick },
  { name: 'Dices', icon: Dices },
  { name: 'Puzzle', icon: Puzzle },
  { name: 'Blocks', icon: Blocks },
  { name: 'Shapes', icon: Shapes },
  { name: 'Hexagon', icon: Hexagon },
  { name: 'Circle', icon: Circle },
  { name: 'Square', icon: Square },
  { name: 'Triangle', icon: Triangle },
  { name: 'Diamond', icon: Diamond },
];

/** 图标名 → 组件映射（O(1) 解析；未知名返回 undefined 由调用方回退默认） */
const ICON_MAP: Map<string, LucideIcon> = new Map(ICON_CHOICES.map((c) => [c.name, c.icon]));

/**
 * 解析设置里的图标名：合法返回组件，空/非法返回 null（调用方回退 APP_TABS 默认图标）。
 * 后端与前端 store 已做脏值清洗，此处为渲染层最终兜底。
 */
export function resolveAppIcon(name: string | null | undefined): LucideIcon | null {
  if (!name) return null;
  return ICON_MAP.get(name) ?? null;
}

/** 各应用内置默认图标名（与 APP_TABS 一致，设置页「恢复默认」用） */
export const DEFAULT_APP_ICONS: Record<AppKey, string> = {
  trae: 'Sparkles',
  buddy: 'Bot',
  qoder: 'Boxes',
};
