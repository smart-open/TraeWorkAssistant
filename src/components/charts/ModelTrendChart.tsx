import {
  CartesianGrid,
  Legend,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
import { useIsDark } from '../../lib/useIsDark';
import { REST_SERIES_KEY, type TrendSeries } from '../../pages/dashboard/modelTrend';

/**
 * 各模型消耗趋势折线图（issue #160）：每个模型一条折线，逐日展示消耗
 * （积分 / 请求数口径由调用方决定并标注）。系列 = Top8 +「其他」聚合（buildModelTrend 生成）。
 * 组件不做空态兜底——空态文案与数据源口径强相关，由调用方展示（§8 诚实标注约定）。
 */

/** 分类调色板（Top8 按序取色）；「其他」固定灰 */
const PALETTE = ['#6366f1', '#f59e0b', '#22c55e', '#38bdf8', '#ec4899', '#14b8a6', '#8b5cf6', '#f97316'];
const REST_COLOR = '#94a3b8';

export default function ModelTrendChart({
  rows,
  series,
  showDots,
  fmtValue,
}: {
  /** 逐日行：label + 各系列数值（缺失日补 0 由调用方负责） */
  rows: Record<string, number | string>[];
  /** 折线系列（按区间累计降序，「其他」在末位） */
  series: TrendSeries[];
  /** 短区间显示数据点 */
  showDots: boolean;
  /** Tooltip 数值格式化 */
  fmtValue: (n: number) => string;
}) {
  const isDark = useIsDark();
  const axisProps = {
    tick: { fontSize: 11, fill: isDark ? '#a1a1aa' : '#94a3b8' },
    axisLine: { stroke: isDark ? '#3f3f46' : '#e2e8f0' },
    tickLine: false,
  } as const;
  const tooltipStyle = {
    fontSize: 12,
    borderRadius: 10,
    border: `1px solid ${isDark ? '#3f3f46' : '#e2e8f0'}`,
    background: isDark ? '#18181b' : '#fff',
    color: isDark ? '#e4e4e7' : '#1e293b',
    boxShadow: '0 6px 16px rgba(0,0,0,0.1)',
    padding: '8px 12px',
  } as const;

  return (
    <div className="h-64">
      <ResponsiveContainer>
        <LineChart data={rows} margin={{ top: 12, right: 16, left: 0, bottom: 4 }}>
          <CartesianGrid strokeDasharray="3 3" stroke={isDark ? '#3f3f46' : '#e2e8f0'} opacity={0.25} vertical={false} />
          <XAxis dataKey="label" {...axisProps} minTickGap={24} />
          <YAxis {...axisProps} axisLine={false} width={56} />
          <Tooltip contentStyle={tooltipStyle} formatter={(v: number, name: string) => [fmtValue(v), name]} />
          <Legend wrapperStyle={{ fontSize: 12 }} />
          {series.map((s, i) => (
            <Line
              key={s.key}
              type="monotone"
              dataKey={s.key}
              name={s.name}
              stroke={s.key === REST_SERIES_KEY ? REST_COLOR : PALETTE[i % PALETTE.length]}
              strokeWidth={1.5}
              dot={showDots ? { r: 2, strokeWidth: 0 } : false}
              activeDot={{ r: 4 }}
              connectNulls
              // 年区间 365 点 × 9 线关闭入场动画，避免切区间卡顿
              isAnimationActive={rows.length > 60 ? false : undefined}
            />
          ))}
        </LineChart>
      </ResponsiveContainer>
    </div>
  );
}
