import { useMetrics } from '@/hooks/use-metrics'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Gauge, Coins, ZapOff, Network } from 'lucide-react'
import type {
  WindowLatency,
  WindowRequestCounts,
} from '@/types/api'

/**
 * 指标明细：把后端按 1m/5m/1h 三窗口聚合的扩展字段全部铺开。
 *
 * 覆盖后端新增但 HealthBanner 未展示的字段：
 * - TTFB 分位（ttfbP50/95/99 + 样本数）
 * - 流式中断 / 救活（streamAborts / streamRecovers）
 * - 客户端可见错误（clientVisibleErrors）
 * - token 统计（input/output/cacheRead）
 * - tier 化代理 fallback（fallbackProxyUsed）
 *
 * 这些字段在旧后端缺失（可选），缺失时显示 "—"。
 */
export function MetricsDetail() {
  const { data } = useMetrics()
  if (!data) return null

  const windows: { label: string; req?: WindowRequestCounts; lat?: WindowLatency; fbProxy?: number }[] = [
    {
      label: '1 分钟',
      req: data.requests.last1m,
      lat: data.latency.last1m,
      fbProxy: data.cooldown.fallbackProxyUsed1m,
    },
    {
      label: '5 分钟',
      req: data.requests.last5m,
      lat: data.latency.last5m,
      fbProxy: data.cooldown.fallbackProxyUsed5m,
    },
    {
      label: '1 小时',
      req: data.requests.last1h,
      lat: data.latency.last1h,
      fbProxy: data.cooldown.fallbackProxyUsed1h,
    },
  ]

  return (
    <div className="grid gap-4 lg:grid-cols-2 mb-6">
      {/* TTFB 分位 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-3">
          <CardTitle className="text-sm font-medium">流式首字节延迟 TTFB</CardTitle>
          <Gauge className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <MetricTable
            windows={windows}
            columns={[
              { header: 'p50', get: (w) => fmtMs(w.lat?.ttfbP50Ms) },
              { header: 'p95', get: (w) => fmtMs(w.lat?.ttfbP95Ms) },
              { header: 'p99', get: (w) => fmtMs(w.lat?.ttfbP99Ms) },
              { header: '样本', get: (w) => fmtNum(w.lat?.ttfbSamples) },
            ]}
          />
        </CardContent>
      </Card>

      {/* 流式稳定性 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-3">
          <CardTitle className="text-sm font-medium">流式稳定性 / 错误</CardTitle>
          <ZapOff className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <MetricTable
            windows={windows}
            columns={[
              { header: '中断', get: (w) => fmtNum(w.req?.streamAborts), warn: (w) => (w.req?.streamAborts ?? 0) > 0 },
              { header: '救活', get: (w) => fmtNum(w.req?.streamRecovers) },
              { header: '客户端错误', get: (w) => fmtNum(w.req?.clientVisibleErrors), warn: (w) => (w.req?.clientVisibleErrors ?? 0) > 0 },
            ]}
          />
        </CardContent>
      </Card>

      {/* Token 用量 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-3">
          <CardTitle className="text-sm font-medium">Token 用量</CardTitle>
          <Coins className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <MetricTable
            windows={windows}
            columns={[
              { header: '输入', get: (w) => fmtTok(w.req?.inputTokensTotal) },
              { header: '输出', get: (w) => fmtTok(w.req?.outputTokensTotal) },
              { header: '缓存读', get: (w) => fmtTok(w.req?.cacheReadTokensTotal) },
            ]}
          />
        </CardContent>
      </Card>

      {/* 代理 fallback */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-3">
          <CardTitle className="text-sm font-medium">Tier 化代理 Fallback</CardTitle>
          <Network className="h-4 w-4 text-muted-foreground" />
        </CardHeader>
        <CardContent>
          <MetricTable
            windows={windows}
            columns={[
              { header: '代理 fallback', get: (w) => fmtNum(w.fbProxy), warn: (w) => (w.fbProxy ?? 0) > 0 },
            ]}
          />
        </CardContent>
      </Card>
    </div>
  )
}

interface WinRow {
  label: string
  req?: WindowRequestCounts
  lat?: WindowLatency
  fbProxy?: number
}

interface Column {
  header: string
  get: (w: WinRow) => string
  warn?: (w: WinRow) => boolean
}

function MetricTable({ windows, columns }: { windows: WinRow[]; columns: Column[] }) {
  return (
    <div className="overflow-x-auto -mx-2">
      <table className="w-full text-xs">
        <thead className="text-muted-foreground border-b">
          <tr>
            <th className="text-left font-normal py-1.5 px-2">窗口</th>
            {columns.map((c) => (
              <th key={c.header} className="text-right font-normal py-1.5 px-2">
                {c.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {windows.map((w) => (
            <tr key={w.label} className="border-b border-border/50 last:border-0">
              <td className="py-1.5 px-2 text-muted-foreground">{w.label}</td>
              {columns.map((c) => (
                <td
                  key={c.header}
                  className={`py-1.5 px-2 text-right tabular-nums font-mono ${
                    c.warn?.(w) ? 'text-amber-600 dark:text-amber-400' : ''
                  }`}
                >
                  {c.get(w)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

function fmtMs(v: number | undefined): string {
  if (v === undefined || v <= 0) return '—'
  return v >= 1000 ? `${(v / 1000).toFixed(1)}s` : `${v}ms`
}

function fmtNum(v: number | undefined): string {
  if (v === undefined) return '—'
  return `${v}`
}

function fmtTok(v: number | undefined): string {
  if (v === undefined) return '—'
  if (v >= 1_000_000) return `${(v / 1_000_000).toFixed(1)}M`
  if (v >= 1_000) return `${(v / 1_000).toFixed(1)}k`
  return `${v}`
}
