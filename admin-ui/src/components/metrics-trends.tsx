import { useMetrics } from '@/hooks/use-metrics'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Activity, Coins, Timer } from 'lucide-react'
import {
  Area,
  AreaChart,
  CartesianGrid,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts'
import type { TimeSeriesPoint } from '@/types/api'

/**
 * 过去 60 分钟时间序列趋势图（数据源 AdminMetricsResponse.timeSeries60m）
 *
 * 三张图：
 * - 请求量：每分钟请求数 / 成功数（面积图）
 * - Token：每分钟 input / output / cache_read token（面积图）
 * - 延迟：每分钟 p50 / TTFB p50（折线图）
 *
 * 后端每分钟 1 桶、固定 60 点，索引 0 = 最旧。5 秒随 useMetrics 轮询刷新。
 */
export function MetricsTrends() {
  const { data } = useMetrics()
  const series = data?.timeSeries60m ?? []

  // 后端没返回（旧版本）或全空：不渲染
  if (series.length === 0) return null

  const chartData = series.map((p: TimeSeriesPoint) => {
    const minsAgo = Math.round(-p.tsOffsetSecs / 60)
    return {
      label: minsAgo === 0 ? 'now' : `-${minsAgo}m`,
      requests: p.requestCount,
      success: p.successCount,
      inputTokens: p.inputTokens,
      outputTokens: p.outputTokens,
      cacheRead: p.cacheReadTokens,
      p50: p.p50Ms,
      ttfb: p.ttfbP50Ms,
    }
  })

  const hasTraffic = chartData.some((d) => d.requests > 0)
  if (!hasTraffic) return null

  return (
    <div className="grid gap-4 lg:grid-cols-3 mb-6">
      <TrendCard
        icon={<Activity className="h-4 w-4 text-muted-foreground" />}
        title="请求量（60min）"
      >
        <ResponsiveContainer width="100%" height={160}>
          <AreaChart data={chartData} margin={{ top: 6, right: 6, left: -20, bottom: 0 }}>
            <defs>
              <linearGradient id="gReq" x1="0" y1="0" x2="0" y2="1">
                <stop offset="5%" stopColor="hsl(217 91% 60%)" stopOpacity={0.4} />
                <stop offset="95%" stopColor="hsl(217 91% 60%)" stopOpacity={0} />
              </linearGradient>
            </defs>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted" vertical={false} />
            <XAxis dataKey="label" tick={{ fontSize: 10 }} interval={11} />
            <YAxis tick={{ fontSize: 10 }} allowDecimals={false} width={28} />
            <Tooltip content={<ChartTooltip unit="" />} />
            <Area
              type="monotone"
              dataKey="requests"
              name="请求"
              stroke="hsl(217 91% 60%)"
              fill="url(#gReq)"
              strokeWidth={2}
            />
            <Area
              type="monotone"
              dataKey="success"
              name="成功"
              stroke="hsl(142 71% 45%)"
              fill="none"
              strokeWidth={1.5}
            />
          </AreaChart>
        </ResponsiveContainer>
      </TrendCard>

      <TrendCard
        icon={<Coins className="h-4 w-4 text-muted-foreground" />}
        title="Token（60min）"
      >
        <ResponsiveContainer width="100%" height={160}>
          <AreaChart data={chartData} margin={{ top: 6, right: 6, left: -8, bottom: 0 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted" vertical={false} />
            <XAxis dataKey="label" tick={{ fontSize: 10 }} interval={11} />
            <YAxis tick={{ fontSize: 10 }} width={40} tickFormatter={formatTokenAxis} />
            <Tooltip content={<ChartTooltip unit=" tok" />} />
            <Area
              type="monotone"
              dataKey="inputTokens"
              name="输入"
              stackId="t"
              stroke="hsl(217 91% 60%)"
              fill="hsl(217 91% 60% / 0.3)"
              strokeWidth={1.5}
            />
            <Area
              type="monotone"
              dataKey="outputTokens"
              name="输出"
              stackId="t"
              stroke="hsl(280 65% 60%)"
              fill="hsl(280 65% 60% / 0.3)"
              strokeWidth={1.5}
            />
            <Area
              type="monotone"
              dataKey="cacheRead"
              name="缓存读"
              stackId="t"
              stroke="hsl(142 71% 45%)"
              fill="hsl(142 71% 45% / 0.3)"
              strokeWidth={1.5}
            />
          </AreaChart>
        </ResponsiveContainer>
      </TrendCard>

      <TrendCard
        icon={<Timer className="h-4 w-4 text-muted-foreground" />}
        title="延迟（60min）"
      >
        <ResponsiveContainer width="100%" height={160}>
          <LineChart data={chartData} margin={{ top: 6, right: 6, left: -12, bottom: 0 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted" vertical={false} />
            <XAxis dataKey="label" tick={{ fontSize: 10 }} interval={11} />
            <YAxis tick={{ fontSize: 10 }} width={36} tickFormatter={(v) => `${v}`} />
            <Tooltip content={<ChartTooltip unit="ms" />} />
            <Line
              type="monotone"
              dataKey="p50"
              name="p50"
              stroke="hsl(217 91% 60%)"
              strokeWidth={2}
              dot={false}
            />
            <Line
              type="monotone"
              dataKey="ttfb"
              name="TTFB"
              stroke="hsl(38 92% 50%)"
              strokeWidth={1.5}
              dot={false}
            />
          </LineChart>
        </ResponsiveContainer>
      </TrendCard>
    </div>
  )
}

function TrendCard({
  icon,
  title,
  children,
}: {
  icon: React.ReactNode
  title: string
  children: React.ReactNode
}) {
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
        <CardTitle className="text-sm font-medium">{title}</CardTitle>
        {icon}
      </CardHeader>
      <CardContent className="px-2">{children}</CardContent>
    </Card>
  )
}

interface TooltipPayloadItem {
  name: string
  value: number
  color: string
}

function ChartTooltip({
  active,
  payload,
  label,
  unit,
}: {
  active?: boolean
  payload?: TooltipPayloadItem[]
  label?: string
  unit: string
}) {
  if (!active || !payload || payload.length === 0) return null
  return (
    <div className="rounded-md border bg-background/95 px-2.5 py-1.5 text-xs shadow-md">
      <div className="mb-1 font-medium text-muted-foreground">{label}</div>
      {payload.map((p) => (
        <div key={p.name} className="flex items-center gap-1.5">
          <span
            className="inline-block h-2 w-2 rounded-full"
            style={{ backgroundColor: p.color }}
          />
          <span className="text-muted-foreground">{p.name}</span>
          <span className="ml-auto font-mono tabular-nums">
            {formatNum(p.value)}
            {unit}
          </span>
        </div>
      ))}
    </div>
  )
}

function formatNum(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(1)}k`
  return `${v}`
}

function formatTokenAxis(v: number): string {
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(0)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(0)}k`
  return `${v}`
}
