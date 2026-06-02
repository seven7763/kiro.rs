import { useMetrics } from '@/hooks/use-metrics'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { BarChart3, KeyRound } from 'lucide-react'
import type { DimensionBreakdown } from '@/types/api'

/**
 * 1 小时窗口下的请求维度切片视图
 *
 * 两张表：
 * - by-model：上行模型 → 请求数 / 成功率 / p50 / p95（top 10）
 * - by-credential：命中凭据 ID → 请求数 / 成功率 / p50 / p95（top 20）
 *
 * 数据来自 [`AdminMetricsResponse.byModel1h`] 与
 * [`AdminMetricsResponse.byCredential1h`]，由 admin metrics endpoint
 * 在 1h 窗口内聚合，按 count 降序。两个字段可选，旧后端缺失时显示 "暂无数据"。
 */
export function MetricsBreakdown() {
  const { data, isLoading } = useMetrics()

  const byModel = data?.byModel1h ?? []
  const byCredential = data?.byCredential1h ?? []

  if (isLoading) {
    return (
      <div className="grid gap-4 md:grid-cols-2 mb-6">
        <BreakdownCard
          icon={<BarChart3 className="h-4 w-4 text-muted-foreground" />}
          title="按模型 1h"
          loading
        />
        <BreakdownCard
          icon={<KeyRound className="h-4 w-4 text-muted-foreground" />}
          title="按凭据 1h"
          loading
        />
      </div>
    )
  }

  // 空数据时不渲染（避免占位）—— 让用户在请求量很低的时段不看到空表
  if (byModel.length === 0 && byCredential.length === 0) {
    return null
  }

  return (
    <div className="grid gap-4 md:grid-cols-2 mb-6">
      <BreakdownCard
        icon={<BarChart3 className="h-4 w-4 text-muted-foreground" />}
        title={`按模型 1h（top ${byModel.length}）`}
        rows={byModel}
        keyHeader="模型"
      />
      <BreakdownCard
        icon={<KeyRound className="h-4 w-4 text-muted-foreground" />}
        title={`按凭据 1h（top ${byCredential.length}）`}
        rows={byCredential}
        keyHeader="凭据 ID"
      />
    </div>
  )
}

interface BreakdownCardProps {
  icon: React.ReactNode
  title: string
  rows?: DimensionBreakdown[]
  keyHeader?: string
  loading?: boolean
}

function BreakdownCard({ icon, title, rows, keyHeader, loading }: BreakdownCardProps) {
  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-3">
        <CardTitle className="text-sm font-medium">{title}</CardTitle>
        {icon}
      </CardHeader>
      <CardContent>
        {loading ? (
          <div className="text-sm text-muted-foreground">加载中…</div>
        ) : rows && rows.length > 0 ? (
          <div className="overflow-x-auto -mx-2">
            <table className="w-full text-xs">
              <thead className="text-muted-foreground border-b">
                <tr>
                  <th className="text-left font-normal py-1.5 px-2">{keyHeader}</th>
                  <th className="text-right font-normal py-1.5 px-2">请求</th>
                  <th className="text-right font-normal py-1.5 px-2">成功率</th>
                  <th className="text-right font-normal py-1.5 px-2">p50</th>
                  <th className="text-right font-normal py-1.5 px-2">p95</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((d) => (
                  <tr
                    key={d.key}
                    className="border-b border-border/50 last:border-0 hover:bg-muted/30"
                  >
                    <td className="py-1.5 px-2 font-mono truncate max-w-[180px]">
                      {d.key}
                    </td>
                    <td className="py-1.5 px-2 text-right tabular-nums">{d.count}</td>
                    <td
                      className={`py-1.5 px-2 text-right tabular-nums ${
                        d.successRate !== null && d.successRate < 90
                          ? 'text-amber-600 dark:text-amber-400'
                          : ''
                      }`}
                    >
                      {d.successRate !== null ? `${d.successRate.toFixed(1)}%` : '—'}
                    </td>
                    <td className="py-1.5 px-2 text-right tabular-nums">
                      {formatLatency(d.p50Ms)}
                    </td>
                    <td className="py-1.5 px-2 text-right tabular-nums">
                      {formatLatency(d.p95Ms)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <div className="text-sm text-muted-foreground">暂无 1h 数据</div>
        )}
      </CardContent>
    </Card>
  )
}

function formatLatency(ms: number): string {
  if (ms <= 0) return '—'
  if (ms >= 1000) return `${(ms / 1000).toFixed(1)}s`
  return `${ms}ms`
}
